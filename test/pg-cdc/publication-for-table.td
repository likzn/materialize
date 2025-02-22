# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

#
# Test that a publication that does not cover all tables is handled correctly
#

> CREATE SECRET pgpass AS 'postgres'
> CREATE CONNECTION pg FOR POSTGRES
  HOST postgres,
  DATABASE postgres,
  USER postgres,
  PASSWORD SECRET pgpass

$ postgres-execute connection=postgres://postgres:postgres@postgres
ALTER USER postgres WITH replication;
DROP SCHEMA IF EXISTS public CASCADE;
DROP PUBLICATION IF EXISTS mz_source;

CREATE SCHEMA public;

CREATE TABLE t1 (f1 INTEGER);
ALTER TABLE t1 REPLICA IDENTITY FULL;
INSERT INTO t1 VALUES (1);

CREATE TABLE t2 (f1 INTEGER);
ALTER TABLE t2 REPLICA IDENTITY FULL;
INSERT INTO t2 VALUES (5);

CREATE PUBLICATION mz_source FOR TABLE t1;

> CREATE MATERIALIZED SOURCE mz_source
  FROM POSTGRES CONNECTION pg
  PUBLICATION 'mz_source';

> CREATE VIEWS FROM SOURCE mz_source (t1);

> SELECT * FROM t1;
1

! CREATE VIEWS FROM SOURCE mz_source (t2);
contains:table t2 not found
