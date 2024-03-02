use std::path::PathBuf;

use crate::data_type::noir_type::NoirType;
use crate::data_type::schema::Schema;
use crate::data_type::stream_item::StreamItem;
use crate::operator::source::CsvOptions;
use crate::{
    box_op::BoxedOperator,
    operator::{filter_expr::FilterExpr, sink::StreamOutput, Operator},
    optimization::dsl::expressions::Expr,
    stream::OptStream,
    Stream,
};

use super::dsl::expressions::AggregateOp;
use super::optimizer::OptimizationOptions;
use super::{
    logical_plan::{JoinType, LogicPlan},
    physical_plan::to_stream,
};

impl<Op> Stream<Op>
where
    Op: Operator<Out = StreamItem> + 'static,
{
    pub fn filter_expr(self, expr: Expr) -> Stream<FilterExpr<Op>> {
        self.add_operator(|prev| FilterExpr::new(prev, expr))
    }

    pub fn select(self, columns: Vec<Expr>) -> Stream<BoxedOperator<StreamItem>> {
        let projections = columns.clone();
        let temp_stream = self.map(move |item| {
            projections
                .iter()
                .map(|expr| expr.evaluate(item.get_value()))
                .collect()
        });
        if columns.iter().any(|e| e.is_aggregator()) {
            let accumulator = columns
                .into_iter()
                .map(|e| e.into_accumulator_state())
                .collect::<Vec<AggregateOp>>();
            temp_stream
                .fold(accumulator, |acc, value: Vec<NoirType>| {
                    for i in 0..acc.len() {
                        acc[i].accumulate(value[i]);
                    }
                })
                .map(|acc| {
                    acc.into_iter()
                        .map(|a| a.finalize())
                        .collect::<Vec<NoirType>>()
                })
                .map(StreamItem::from)
                .into_box()
        } else {
            temp_stream.map(StreamItem::from).into_box()
        }
    }

    pub fn group_by_expr(self, keys: Vec<Expr>) -> Stream<BoxedOperator<StreamItem>> {
        self.group_by(move |item: &StreamItem| {
            keys.iter().map(|k| k.evaluate(item.get_value())).collect()
        })
        .0
        .map(|(k, v)| v.absorb_key(k))
        .into_box()
    }
}

impl OptStream {
    pub fn collect_vec(self) -> StreamOutput<Vec<StreamItem>> {
        let optimized = self.logic_plan.collect_vec().optimize(self.optimizations);
        info!("Optimized plan: {}", optimized);
        to_stream(optimized, self.inner, self.csv_options.unwrap_or_default()).into_output()
    }

    pub fn filter(self, predicate: Expr) -> Self {
        OptStream {
            logic_plan: self.logic_plan.filter(predicate),
            ..self
        }
    }

    pub fn shuffle(self) -> Self {
        OptStream {
            logic_plan: self.logic_plan.shuffle(),
            ..self
        }
    }

    pub fn group_by<E: AsRef<[Expr]>>(self, key: E) -> Self {
        OptStream {
            logic_plan: self.logic_plan.group_by(key),
            ..self
        }
    }

    pub fn select<E: AsRef<[Expr]>>(self, exprs: E) -> Self {
        OptStream {
            logic_plan: self.logic_plan.select(exprs),
            ..self
        }
    }

    pub fn drop_key(self) -> Self {
        OptStream {
            logic_plan: self.logic_plan.drop_key(),
            ..self
        }
    }

    pub fn drop(self, cols: Vec<usize>) -> Self {
        OptStream {
            logic_plan: self.logic_plan.drop(cols),
            ..self
        }
    }

    pub fn join<E: AsRef<[Expr]>>(self, other: OptStream, left_on: E, right_on: E) -> OptStream {
        OptStream {
            logic_plan: self
                .logic_plan
                .join(other.logic_plan, left_on, right_on, JoinType::Inner),

            ..self
        }
    }

    pub fn left_join<E: AsRef<[Expr]>>(
        self,
        other: OptStream,
        left_on: E,
        right_on: E,
    ) -> OptStream {
        OptStream {
            logic_plan: self
                .logic_plan
                .join(other.logic_plan, left_on, right_on, JoinType::Left),
            ..self
        }
    }

    pub fn full_join<E: AsRef<[Expr]>>(
        self,
        other: OptStream,
        left_on: E,
        right_on: E,
    ) -> OptStream {
        OptStream {
            logic_plan: self
                .logic_plan
                .join(other.logic_plan, left_on, right_on, JoinType::Outer),
            ..self
        }
    }

    pub fn with_schema(mut self, schema: Schema) -> Self {
        self.logic_plan.set_schema(schema);
        self
    }

    pub fn with_optimizations(self, optimizations: OptimizationOptions) -> Self {
        Self {
            optimizations,
            ..self
        }
    }

    pub fn with_compiled_expressions(self, compiled: bool) -> Self {
        Self {
            optimizations: self.optimizations.with_compile_expressions(compiled),
            ..self
        }
    }

    pub fn with_predicate_pushdown(self, predicate_pushdown: bool) -> Self {
        Self {
            optimizations: self
                .optimizations
                .with_predicate_pushdown(predicate_pushdown),
            ..self
        }
    }

    pub fn with_projection_pushdown(self, projection_pushdown: bool) -> Self {
        Self {
            optimizations: self
                .optimizations
                .with_projection_pushdown(projection_pushdown),
            ..self
        }
    }

    pub fn with_csv_options(self, csv_options: CsvOptions) -> Self {
        Self {
            csv_options: Some(csv_options),
            ..self
        }
    }
}

impl crate::StreamEnvironment {
    pub fn stream_csv_optimized(&mut self, path: impl Into<PathBuf>) -> OptStream {
        OptStream {
            inner: self.inner.clone(),
            logic_plan: LogicPlan::TableScan {
                path: path.into(),
                predicate: None,
                projections: None,
                schema: None,
            },
            optimizations: OptimizationOptions::default(),
            csv_options: None,
        }
    }

    pub fn stream_par_optimized(
        &mut self,
        generator: fn(u64, u64) -> Box<dyn Iterator<Item = StreamItem> + Send>,
        schema: Schema,
    ) -> OptStream {
        OptStream {
            inner: self.inner.clone(),
            logic_plan: LogicPlan::ParallelIterator { generator, schema },
            optimizations: OptimizationOptions::default(),
            csv_options: None,
        }
    }
}
