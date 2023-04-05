// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::Ordering;
use std::fmt::Display;
use std::fmt::Formatter;
use std::sync::Arc;

use common_catalog::table_context::TableContext;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::types::DataType;
use common_expression::types::NumberDataType;

use super::AggregateFunction;
use crate::binder::WindowOrderByInfo;
use crate::optimizer::ColumnSet;
use crate::optimizer::Distribution;
use crate::optimizer::PhysicalProperty;
use crate::optimizer::RelExpr;
use crate::optimizer::RelationalProperty;
use crate::optimizer::RequiredProperty;
use crate::optimizer::Statistics;
use crate::plans::Operator;
use crate::plans::RelOp;
use crate::plans::ScalarItem;
use crate::IndexType;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Window {
    // aggregate scalar expressions, such as: sum(col1), count(*);
    // or general window functions, such as: row_number(), rank();
    pub index: IndexType,
    pub function: WindowFuncType,

    // partition by scalar expressions
    pub partition_by: Vec<ScalarItem>,
    // order by
    pub order_by: Vec<WindowOrderByInfo>,
    // window frames
    pub frame: WindowFuncFrame,
}

impl Window {
    pub fn used_columns(&self) -> Result<ColumnSet> {
        let mut used_columns = ColumnSet::new();

        used_columns.insert(self.index);

        if let WindowFuncType::Aggregate(agg) = &self.function {
            for scalar in &agg.args {
                used_columns = used_columns
                    .union(&scalar.used_columns())
                    .cloned()
                    .collect();
            }
        }

        for part in self.partition_by.iter() {
            used_columns.insert(part.index);
            used_columns.extend(part.scalar.used_columns())
        }

        for sort in self.order_by.iter() {
            used_columns.insert(sort.order_by_item.index);
            used_columns.extend(sort.order_by_item.scalar.used_columns())
        }

        Ok(used_columns)
    }
}

impl Operator for Window {
    fn rel_op(&self) -> RelOp {
        RelOp::Window
    }

    fn derive_physical_prop(&self, rel_expr: &RelExpr) -> Result<PhysicalProperty> {
        rel_expr.derive_physical_prop_child(0)
    }

    fn compute_required_prop_child(
        &self,
        _ctx: Arc<dyn TableContext>,
        _rel_expr: &RelExpr,
        _child_index: usize,
        required: &RequiredProperty,
    ) -> Result<RequiredProperty> {
        let mut required = required.clone();
        required.distribution = Distribution::Serial;
        Ok(required)
    }

    fn derive_relational_prop(&self, rel_expr: &RelExpr) -> Result<RelationalProperty> {
        let input_prop = rel_expr.derive_relational_prop_child(0)?;

        // Derive output columns
        let output_columns = ColumnSet::from([self.index]);

        // Derive outer columns
        let outer_columns = input_prop
            .outer_columns
            .difference(&output_columns)
            .cloned()
            .collect();

        let cardinality = if self.partition_by.is_empty() {
            // Scalar aggregation
            1.0
        } else if self.partition_by.iter().any(|item| {
            input_prop
                .statistics
                .column_stats
                .get(&item.index)
                .is_none()
        }) {
            input_prop.cardinality
        } else {
            // A upper bound
            let res = self.partition_by.iter().fold(1.0, |acc, item| {
                let item_stat = input_prop.statistics.column_stats.get(&item.index).unwrap();
                acc * item_stat.ndv
            });
            // To avoid res is very large
            f64::min(res, input_prop.cardinality)
        };

        let precise_cardinality = if self.partition_by.is_empty() {
            Some(1)
        } else {
            None
        };

        // Derive used columns
        let mut used_columns = self.used_columns()?;
        used_columns.extend(input_prop.used_columns);
        let column_stats = input_prop.statistics.column_stats;
        let is_accurate = input_prop.statistics.is_accurate;

        Ok(RelationalProperty {
            output_columns,
            outer_columns,
            used_columns,
            cardinality,
            statistics: Statistics {
                precise_cardinality,
                column_stats,
                is_accurate,
            },
        })
    }
}

#[derive(Default, Clone, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct WindowFuncFrame {
    pub units: WindowFuncFrameUnits,
    pub start_bound: WindowFuncFrameBound,
    pub end_bound: WindowFuncFrameBound,
}

impl Display for WindowFuncFrame {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}: {:?} ~ {:?}",
            self.units, self.start_bound, self.end_bound
        )
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum WindowFuncFrameUnits {
    #[default]
    Rows,
    Range,
}

#[derive(Default, Clone, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub enum WindowFuncFrameBound {
    /// `CURRENT ROW`
    #[default]
    CurrentRow,
    /// `<N> PRECEDING` or `UNBOUNDED PRECEDING`
    Preceding(Option<usize>),
    /// `<N> FOLLOWING` or `UNBOUNDED FOLLOWING`.
    Following(Option<usize>),
}

impl WindowFuncFrameBound {
    fn to_number(&self) -> i64 {
        match self {
            WindowFuncFrameBound::CurrentRow => 0,
            WindowFuncFrameBound::Preceding(n) => match n {
                None => i64::MIN,
                Some(n) => -(*n as i64),
            },
            WindowFuncFrameBound::Following(n) => match n {
                None => i64::MAX,
                Some(n) => *n as i64,
            },
        }
    }
}

impl PartialOrd for WindowFuncFrameBound {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.to_number().partial_cmp(&other.to_number())
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum WindowFuncType {
    Aggregate(AggregateFunction),
    RowNumber,
    Rank,
    DenseRank,
}

impl WindowFuncType {
    pub fn from_name(name: &str) -> Result<WindowFuncType> {
        match name {
            "row_number" => Ok(WindowFuncType::RowNumber),
            "rank" => Ok(WindowFuncType::Rank),
            "dense_rank" => Ok(WindowFuncType::DenseRank),
            _ => Err(ErrorCode::UnknownFunction(format!(
                "Unknown window function: {}",
                name
            ))),
        }
    }
    pub fn func_name(&self) -> String {
        match self {
            WindowFuncType::Aggregate(agg) => agg.func_name.to_string(),
            WindowFuncType::RowNumber => "row_number".to_string(),
            WindowFuncType::Rank => "rank".to_string(),
            WindowFuncType::DenseRank => "dense_rank".to_string(),
        }
    }

    pub fn used_columns(&self) -> ColumnSet {
        match self {
            WindowFuncType::Aggregate(agg) => {
                agg.args.iter().flat_map(|arg| arg.used_columns()).collect()
            }
            _ => ColumnSet::new(),
        }
    }

    pub fn return_type(&self) -> DataType {
        match self {
            WindowFuncType::Aggregate(agg) => *agg.return_type.clone(),
            WindowFuncType::RowNumber | WindowFuncType::Rank | WindowFuncType::DenseRank => {
                DataType::Number(NumberDataType::UInt64)
            }
        }
    }
}