# Query Compilation 101

## Prerequisite videos

* [Materialized performance 101](https://drive.google.com/file/d/1BlCFHVJsi6-YfQQpPMaWOhDOvaQDdcWV/view?usp=sharing)
  (skip forward to 5:13)
* [Materialized internals 101](https://drive.google.com/file/d/1_SlM-zQR2FifNMeECnFTnwRcTc7zxRuc/view).
* [Introduction to one-off queries](https://drive.google.com/file/d/1LsyMY1OMmDS7uQS6cT6IFmdROGiPB4Im/view?usp=sharing).
* [Materialize Decorrelation explained in Jamie Brandon’s Blog](https://www.scattered-thoughts.net/writing/materialize-decorrelation/).

## Current Compiler Pipeline

Representations:

* `SQL` — source language
* [`AST`](https://github.com/MaterializeInc/materialize/blob/main/src/sql/src/plan.rs) — a parsed version of a SQL query.
* [`HIR`](https://github.com/MaterializeInc/materialize/blob/main/src/sql/src/plan/expr.rs) — high-level intermediate representation.
* [`MIR`](https://github.com/MaterializeInc/materialize/blob/main/src/expr/src/relation/mod.rs) — mid-level intermediate representation.
* [`LIR`](https://github.com/MaterializeInc/materialize/blob/main/src/compute-client/src/plan/mod.rs) — low-level intermediate representation.
* `TDO` — target language (timely & differential operators).

Transformations in the compile-time lifecycle of a dataflow.

* [`SQL ⇒ AST`](https://github.com/materializeinc/materialize/blob/main/src/sql-parser/src/parser.rs#L55).
    * Parsing the SQL query.
* [`AST ⇒ AST`](https://github.com/MaterializeInc/materialize/blob/main/src/adapter/src/coord.rs#L1876)
    * [Resolving names against the catalog.](https://github.com/MaterializeInc/materialize/blob/main/src/sql/src/names.rs#L1035-L1053)
        * [`CatalogItemType`](https://github.com/MaterializeInc/materialize/blob/main/src/sql/src/catalog.rs#L336)
            lists the kinds of objects that can be resolved against the catalog.
* [`AST ⇒ HIR`](https://github.com/MaterializeInc/materialize/blob/main/src/sql/src/plan/query.rs#L90-L129).
    * [Resolving column references, column aliases, and table aliases](https://github.com/MaterializeInc/materialize/blob/main/src/sql/src/plan/scope.rs)
    * If the SQL query is a one-off, the outermost `TopK` is converted to a
      RowSetFinishing at this point.
    * `EXPLAIN RAW PLAN` returns the result of transformations up to this point.
* [`HIR ⇒ HIR`](https://github.com/MaterializeInc/materialize/blob/main/src/sql/src/plan/lowering.rs#L149-L150)
    * Predecorrelation rewrites:
        * [Split out subquery conditions out as a separate predicate.](https://github.com/MaterializeInc/materialize/blob/main/src/sql/src/plan/transform_expr.rs#L54)
        * [Try to rewrite other types of subqueries into EXISTS subqueries.](https://github.com/MaterializeInc/materialize/blob/main/src/sql/src/plan/transform_expr.rs#L156)
* [`HIR ⇒ MIR`](https://github.com/MaterializeInc/materialize/blob/main/src/sql/src/plan/lowering.rs).
    * Decorrelation:
        * Correlated queries are rewritten as graphs with join and distinct.
    * Lowering — express SQL-specific concepts as dataflow sub-graphs:
        * Outer joins are decomposed into multiple inner joins ([see README.md](https://github.com/aalexandrov/mzt-repos/blob/main/simplify_outer_joins/README.md)).
        * Machinery for introducing defaults in empty global aggregates.
        * Machinery for introducing errors for `SELECT` subqueries with more than one return value.
    * `EXPLAIN DECORRELATED PLAN` returns the result of transformations up to this point.
* [`MIR ⇒ MIR`](https://github.com/MaterializeInc/materialize/blob/main/src/transform).
    * [If the query is a view
      definition](https://github.com/MaterializeInc/materialize/blob/main/src/adapter/src/catalog.rs#L3325),
      run per-view logical optimizations against the SQL query. The catalog
      stores the result of transformations up to this point.
    * [Construct a dataflow for the query](https://github.com/MaterializeInc/materialize/blob/main/src/adapter/src/coord/dataflow_builder.rs):
        * If the query depends on not-materialized views, the definitions of the
          not-materialized views get inlined.
        * For each materialized view that a query depends on, import all of its
          materializations. (This corresponds to all indexes on that view, which
          you can see if you call `SHOW INDEXES IN <view>`).
    * Run optimizations against the dataflow:
        * [Per-view logical](https://github.com/MaterializeInc/materialize/blob/main/src/transform/src/lib.rs#L282-L337).
        * [Cross-view logical](https://github.com/MaterializeInc/materialize/blob/main/src/transform/src/dataflow.rs#L31-L60).
            * Propagating source information up: optimize_dataflow_monotonic
            * Pushing optimizations down to sources: `LinearOperators`
                * [CSV decoding](https://github.com/MaterializeInc/materialize/blob/main/src/dataflow/src/decode/csv.rs)
                * [Upsert](https://github.com/MaterializeInc/materialize/blob/main/src/dataflow/src/render/upsert.rs)
            * View inlining.
            * Theoretically supports producing more than one index/sink in the same dataflow.
        * [Per-view logical](https://github.com/MaterializeInc/materialize/blob/main/src/transform/src/lib.rs#L281-L337) (second round).
        * [Per-view physical](https://github.com/MaterializeInc/materialize/blob/main/src/transform/src/lib.rs#L345-L367).
    * `EXPLAIN OPTIMIZED PLAN` returns the result of transformations up to this point.
* [`MIR ⇒ LIR`](https://github.com/MaterializeInc/materialize/blob/main/src/compute-client/src/plan/mod.rs#L882-L897).
    * Decisions are made regarding rendering.
        * All aggregations are created equal in MIR, but from the rendering perspective, [aggregations are evaluated differently according to what data needs to be kept to recalculate the aggregation after receiving a diff](https://github.com/MaterializeInc/materialize/blob/main/src/compute-client/src/plan/reduce.rs). A pictorial version can be found [here](https://github.com/MaterializeInc/materialize/blob/main/doc/developer/arrangements.md).
        * [Joins are broken down into multiple stages](https://github.com/MaterializeInc/materialize/blob/main/src/compute-client/src/plan/join/linear_join.rs), and filters + projects run between each stage to shrink the intermediate result.
    * RelationTypes (column types + unique keys) are discarded since we do no key or type of validation at render time.
    * `EXPLAIN PHYSICAL PLAN` returns the result of transformations up to this point.
* [`LIR ⇒ TDO`](https://github.com/MaterializeInc/materialize/blob/main/src/compute/src/render/mod.rs).

For a one-off query, we run all the transformations until the LIR stage. Then we
determine whether we need to serve the query on the "slow path", that is,
creating a temporary dataflow and then deleting it. If we don't need to serve
the query on the "slow path", then we can skip the `LIR ⇒ TDO` step.
Existing "fast paths" include:
* [reading from an existing dataflow.](https://github.com/MaterializeInc/materialize/blob/main/src/compute/src/compute_state.rs#L689)
* [the adapter itself spitting out a constant set of rows.](https://github.com/MaterializeInc/materialize/blob/main/src/adapter/src/coord.rs#L6307)

Currently, the optimization team is mostly concerned with the `HIR ⇒ MIR` and `MIR ⇒ MIR` stages.

<!--
# Future Pipeline

[Diagram](https://docs.google.com/drawings/d/1Fil1-oYy3PkP3bD7WoZphW319Pj60cMAs2HcS9A21uo/edit)

[Design doc](https://github.com/MaterializeInc/materialize/blob/main/doc/developer/design/20210707_qgm_sql_high_level_representation.md)

* [`SQL ⇒ AST`](https://github.com/MaterializeInc/materialize/blob/main/src/sql-parser).
    * Parsing the SQL query
* `AST ⇒ QGM`.
    * Name resolution.
* `QGM ⇒ QGM`.
    * Optimizing rewrites + decorrelation + more optimizing rewrites.
* `QGM ⇒ MIR`.
    * Lowering.
* [`MIR ⇒ MIR`](https://github.com/MaterializeInc/materialize/blob/main/src/transform).
    * Optimizations. What this looks like is to be determined.
        * Some optimizations may become redundant after optimizing rewrites are added.
        * Note that we may be able to eliminate the per-view/cross-view distinction by modifying MIR to have more than one starting point.
* `MIR ⇒ LIR`.
* `LIR ⇒ TDO`.
-->

## Testing

### Integration tests

* [Sqllogictest](https://github.com/MaterializeInc/materialize/blob/main/doc/developer/sqllogictest.md)
    * [Philip’s RQG tests](https://docs.google.com/presentation/d/1PvUzdeblYwLIWMpBLCtKY1Gys4L92jmr7fTdo4zE2g4/edit) will be in this format.
        * Add Philip to any PR where query plans may change.
    * A PR can be merged if it passes Fast SLT.
    * A PR does not need to pass Full SLT tests (`test/sqllogictest/sqlite`) to be merged.
        * Full SLT tests take 2-3 hours.
        * You can manually initiate full SLT tests on your branch [here](https://buildkite.com/materialize/sql-logic-tests).
* [Testdrive](https://github.com/MaterializeInc/materialize/blob/main/doc/developer/testdrive.md)
    * We generally do not use testdrive except to see [linear operators in action](https://github.com/MaterializeInc/materialize/blob/main/test/testdrive/source-linear-operators.td).

### Unit tests

* [Datadriven](https://github.com/MaterializeInc/materialize/blob/main/doc/developer/guide-testing.md#datadriven)
    * [Transform unit tests ](https://github.com/MaterializeInc/materialize/tree/main/src/transform)currently allow:
        * testing each transformation independently of the others.
        * Printing out which block of transformations change the plan and how.
    * [Unit tests in the mz-expr](https://github.com/MaterializeInc/materialize/tree/main/src/expr/tests) crate currently allow:
        * Testing the simplifying MirScalarExpr, predicates, join equivalences.
        * Testing MapFilterProject.
    * [There is a DSL to specifying arbitrary MIRs.](https://github.com/MaterializeInc/materialize/tree/main/src/expr-test-util)
    * [DSL to specify arbitrary enums and structs.](https://github.com/MaterializeInc/materialize/tree/main/src/lowertest)

### Performance tests

* [TPCH](https://materializeinc.slack.com/archives/C01BE3RN82F/p1611161615021000)

## Tooling

* [mzt](https://github.com/aalexandrov/mzt) — can be used to create repositories of plans and write up a markdown that explains something based on those plans (see Alexander’s [mzt-repos](https://github.com/aalexandrov/mzt-repos) for example).
