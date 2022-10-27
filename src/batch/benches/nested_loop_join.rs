// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
pub mod utils;
use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use futures::StreamExt;
use risingwave_batch::executor::test_utils::{gen_data, MockExecutor};
use risingwave_batch::executor::{BoxedExecutor, JoinType, NestedLoopJoinExecutor};
use risingwave_common::catalog::schema_test_utils::field_n;
use risingwave_common::types::{DataType, ScalarImpl};
use risingwave_expr::expr::build_from_prost;
use risingwave_pb::data::data_type::TypeName;
use risingwave_pb::expr::expr_node::RexNode;
use risingwave_pb::expr::expr_node::Type::{
    ConstantValue as TConstValue, Equal, InputRef, Modulus,
};
use risingwave_pb::expr::{ConstantValue, ExprNode, FunctionCall, InputRefExpr};
use utils::{bench_join, create_input};
use tokio::runtime::Runtime;

fn create_nested_loop_join_executor(
    join_type: JoinType,
    left_chunk_size: usize,
    left_chunk_num: usize,
    right_chunk_size: usize,
    right_chunk_num: usize,
) -> BoxedExecutor {
    let left_input = gen_data(left_chunk_size, left_chunk_num, &[DataType::Int64]);
    let right_input = gen_data(right_chunk_size, right_chunk_num, &[DataType::Int64]);

    let mut left_child = Box::new(MockExecutor::new(field_n::<1>(DataType::Int64)));
    left_input.into_iter().for_each(|c| left_child.add(c));

    let mut right_child = Box::new(MockExecutor::new(field_n::<1>(DataType::Int64)));
    right_input.into_iter().for_each(|c| right_child.add(c));

    // Expression: $1 % 2 == $2 % 3
    let join_expr = {
        let left_input_ref = ExprNode {
            expr_type: InputRef as i32,
            return_type: Some(risingwave_pb::data::DataType {
                type_name: TypeName::Int64 as i32,
                ..Default::default()
            }),
            rex_node: Some(RexNode::InputRef(InputRefExpr { column_idx: 0 })),
        };

        let right_input_ref = ExprNode {
            expr_type: InputRef as i32,
            return_type: Some(risingwave_pb::data::DataType {
                type_name: TypeName::Int64 as i32,
                ..Default::default()
            }),
            rex_node: Some(RexNode::InputRef(InputRefExpr { column_idx: 1 })),
        };

        let literal2 = ExprNode {
            expr_type: TConstValue as i32,
            return_type: Some(risingwave_pb::data::DataType {
                type_name: TypeName::Int64 as i32,
                ..Default::default()
            }),
            rex_node: Some(RexNode::Constant(ConstantValue {
                body: ScalarImpl::Int64(2).to_protobuf(),
            })),
        };

        let literal3 = ExprNode {
            expr_type: TConstValue as i32,
            return_type: Some(risingwave_pb::data::DataType {
                type_name: TypeName::Int64 as i32,
                ..Default::default()
            }),
            rex_node: Some(RexNode::Constant(ConstantValue {
                body: ScalarImpl::Int64(3).to_protobuf(),
            })),
        };

        // $1 % 2
        let left_mod2 = ExprNode {
            expr_type: Modulus as i32,
            return_type: Some(risingwave_pb::data::DataType {
                type_name: TypeName::Int64 as i32,
                ..Default::default()
            }),
            rex_node: Some(RexNode::FuncCall(FunctionCall {
                children: vec![left_input_ref, literal2],
            })),
        };

        // $2 % 3
        let right_mod3 = ExprNode {
            expr_type: Modulus as i32,
            return_type: Some(risingwave_pb::data::DataType {
                type_name: TypeName::Int64 as i32,
                ..Default::default()
            }),
            rex_node: Some(RexNode::FuncCall(FunctionCall {
                children: vec![right_input_ref, literal3],
            })),
        };

        // $1 % 2 == $2 % 3
        ExprNode {
            expr_type: Equal as i32,
            return_type: Some(risingwave_pb::data::DataType {
                type_name: TypeName::Boolean as i32,
                ..Default::default()
            }),
            rex_node: Some(RexNode::FuncCall(FunctionCall {
                children: vec![left_mod2, right_mod3],
            })),
        }
    };

    let output_indices = match join_type {
        JoinType::LeftSemi | JoinType::LeftAnti => vec![0],
        JoinType::RightSemi | JoinType::RightAnti => vec![0],
        _ => vec![0, 1],
    };

    Box::new(NestedLoopJoinExecutor::new(
        build_from_prost(&join_expr).unwrap(),
        join_type,
        output_indices,
        left_child,
        right_child,
        "NestedLoopJoinExecutor".into(),
    ))
}

async fn execute_nested_loop_join_executor(executor: BoxedExecutor) {
    let mut stream = executor.execute();
    while let Some(ret) = stream.next().await {
        black_box(ret.unwrap());
    }
}

fn bench_nested_loop_join(c: &mut Criterion) {
    const LEFT_SIZE: usize = 2 * 1024;
    const RIGHT_SIZE: usize = 2 * 1024;
    let rt = Runtime::new().unwrap();
    for join_type in &[
        JoinType::Inner,
        JoinType::LeftOuter,
        JoinType::LeftSemi,
        JoinType::LeftAnti,
        JoinType::RightOuter,
        JoinType::RightSemi,
        JoinType::RightAnti,
        JoinType::FullOuter,
    ] {
        for chunk_size in &[32, 128, 512, 1024] {
            c.bench_with_input(
                BenchmarkId::new(
                    "NestedLoopJoinExecutor",
                    format!("{}({:?})", chunk_size, join_type),
                ),
                chunk_size,
                |b, &chunk_size| {
                    let left_chunk_num = LEFT_SIZE / chunk_size;
                    let right_chunk_num = RIGHT_SIZE / chunk_size;
                    b.to_async(&rt).iter_batched(
                        || {
                            create_nested_loop_join_executor(
                                *join_type,
                                chunk_size,
                                left_chunk_num,
                                chunk_size,
                                right_chunk_num,
                            )
                        },
                        |e| execute_nested_loop_join_executor(e),
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }
}

criterion_group!(benches, bench_nested_loop_join);
criterion_main!(benches);
