# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

# Test monotonicity analyses which derive from ENVELOPE NONE sources.
# Note that these only test the implementation for monotonic sources,
# they do not test that the analysis doesn't have false positives on
# non-monotonic sources.

$ set non-dbz-schema={
    "type": "record",
    "name": "cpx",
    "fields": [
      {"name": "a", "type": "long"},
      {"name": "b", "type": "long"}
    ]
  }

$ kafka-create-topic topic=non-dbz-data

$ kafka-ingest format=avro topic=non-dbz-data schema=${non-dbz-schema} timestamp=1
{"a": 1, "b": 2}
{"a": 1, "b": 1048576}
{"a": 2, "b": 3}
{"a": 2, "b": 4}

> CREATE SOURCE non_dbz_data
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-non-dbz-data-${testdrive.seed}'
  FORMAT AVRO USING SCHEMA '${non-dbz-schema}'
  ENVELOPE NONE

> CREATE MATERIALIZED VIEW monotonic_min AS SELECT a, min(b) FROM non_dbz_data group by a

> SELECT * FROM monotonic_min
a min
---
1 2
2 3

> CREATE MATERIALIZED VIEW monotonic_max AS SELECT a, max(b) FROM non_dbz_data group by a

> SELECT * FROM monotonic_max
a max
---
1 1048576
2 4

# Smoke test for aggregate fusion
> CREATE MATERIALIZED VIEW monotonic_fused AS SELECT a, min(b), max(b + 1)  FROM non_dbz_data group by a

> SELECT * FROM monotonic_fused
a min max
---
1 2 1048577
2 3 5

# Propagating monotonicity analysis through materialized views
# TODO: After https://github.com/MaterializeInc/materialize/pull/13238 is merged, modify these tests to dig into
# the plans with `jq` and just check for `MonotonicTopK` being present.

> CREATE MATERIALIZED VIEW m1 AS SELECT b FROM non_dbz_data

> CREATE VIEW v2 AS SELECT * FROM m1 ORDER BY b LIMIT 3

$ set-regex match=(.User.:\s\d+|\(u\d+\)) replacement=UID

? EXPLAIN PHYSICAL PLAN FOR VIEW v2
{
  "TopK": {
    "input": {
      "ArrangeBy": {
        "input": {
          "Get": {
            "id": {
              "Global": {
                UID
              }
            },
            "keys": {
              "raw": false,
              "arranged": [
                [
                  [
                    {
                      "Column": 0
                    }
                  ],
                  {
                    "0": 0
                  },
                  []
                ]
              ]
            },
            "plan": "PassArrangements"
          }
        },
        "forms": {
          "raw": true,
          "arranged": []
        },
        "input_key": [
          {
            "Column": 0
          }
        ],
        "input_mfp": {
          "expressions": [],
          "predicates": [],
          "projection": [
            0
          ],
          "input_arity": 1
        }
      }
    },
    "top_k_plan": {
      "MonotonicTopK": {
        "group_key": [],
        "order_key": [
          {
            "column": 0,
            "desc": false,
            "nulls_last": true
          }
        ],
        "limit": 3,
        "arity": 1
      }
    }
  }
}

> CREATE MATERIALIZED SOURCE non_dbz_data_materialized
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-non-dbz-data-${testdrive.seed}'
  FORMAT AVRO USING SCHEMA '${non-dbz-schema}'
  ENVELOPE NONE

# Propagating monotonicity analysis from a materialized source

> CREATE VIEW v3 AS SELECT * FROM non_dbz_data_materialized ORDER BY b LIMIT 3

? EXPLAIN PHYSICAL PLAN FOR VIEW v3
{
  "TopK": {
    "input": {
      "ArrangeBy": {
        "input": {
          "Get": {
            "id": {
              "Global": {
                UID
              }
            },
            "keys": {
              "raw": false,
              "arranged": [
                [
                  [
                    {
                      "Column": 0
                    },
                    {
                      "Column": 1
                    }
                  ],
                  {
                    "0": 0,
                    "1": 1
                  },
                  []
                ]
              ]
            },
            "plan": "PassArrangements"
          }
        },
        "forms": {
          "raw": true,
          "arranged": []
        },
        "input_key": [
          {
            "Column": 0
          },
          {
            "Column": 1
          }
        ],
        "input_mfp": {
          "expressions": [],
          "predicates": [],
          "projection": [
            0,
            1
          ],
          "input_arity": 2
        }
      }
    },
    "top_k_plan": {
      "MonotonicTopK": {
        "group_key": [],
        "order_key": [
          {
            "column": 1,
            "desc": false,
            "nulls_last": true
          }
        ],
        "limit": 3,
        "arity": 2
      }
    }
  }
}


> CREATE MATERIALIZED VIEW m4 AS SELECT b+1 as c FROM m1

# Propagating monotonicity analysis through 2 materialized views (m1 and m4)

> CREATE VIEW v5 AS SELECT * from m4 ORDER BY c LIMIT 2

? EXPLAIN PHYSICAL PLAN FOR VIEW v5
{
  "TopK": {
    "input": {
      "ArrangeBy": {
        "input": {
          "Get": {
            "id": {
              "Global": {
                UID
              }
            },
            "keys": {
              "raw": false,
              "arranged": [
                [
                  [
                    {
                      "Column": 0
                    }
                  ],
                  {
                    "0": 0
                  },
                  []
                ]
              ]
            },
            "plan": "PassArrangements"
          }
        },
        "forms": {
          "raw": true,
          "arranged": []
        },
        "input_key": [
          {
            "Column": 0
          }
        ],
        "input_mfp": {
          "expressions": [],
          "predicates": [],
          "projection": [
            0
          ],
          "input_arity": 1
        }
      }
    },
    "top_k_plan": {
      "MonotonicTopK": {
        "group_key": [],
        "order_key": [
          {
            "column": 0,
            "desc": false,
            "nulls_last": true
          }
        ],
        "limit": 2,
        "arity": 1
      }
    }
  }
}

> CREATE MATERIALIZED VIEW m6 AS SELECT c FROM v5

# Non-monotonic materialized view -- v7 can't use a monotonic TopK plan, because m6 and v5 are not monotonic

> CREATE VIEW v7 AS SELECT * from m6 ORDER BY c LIMIT 2

? EXPLAIN PHYSICAL PLAN FOR VIEW v7
{
  "TopK": {
    "input": {
      "ArrangeBy": {
        "input": {
          "Get": {
            "id": {
              "Global": {
                UID
              }
            },
            "keys": {
              "raw": false,
              "arranged": [
                [
                  [
                    {
                      "Column": 0
                    }
                  ],
                  {
                    "0": 0
                  },
                  []
                ]
              ]
            },
            "plan": "PassArrangements"
          }
        },
        "forms": {
          "raw": true,
          "arranged": []
        },
        "input_key": [
          {
            "Column": 0
          }
        ],
        "input_mfp": {
          "expressions": [],
          "predicates": [],
          "projection": [
            0
          ],
          "input_arity": 1
        }
      }
    },
    "top_k_plan": {
      "Basic": {
        "group_key": [],
        "order_key": [
          {
            "column": 0,
            "desc": false,
            "nulls_last": true
          }
        ],
        "limit": 2,
        "offset": 0,
        "arity": 1
      }
    }
  }
}


> CREATE MATERIALIZED VIEW m8 AS SELECT * from (SELECT * FROM m1 UNION ALL SELECT * FROM m1)

> CREATE MATERIALIZED VIEW m9 AS SELECT * from (SELECT * FROM m1 UNION ALL SELECT * FROM m8)

> CREATE VIEW v10 as SELECT b FROM m9 ORDER BY b LIMIT 2;

# Propagating monotonicity analysis in a complex situation: (m1, m1) -> m8; (m1, m8) -> m9

? EXPLAIN PHYSICAL PLAN FOR VIEW v10;
{
  "TopK": {
    "input": {
      "ArrangeBy": {
        "input": {
          "Get": {
            "id": {
              "Global": {
                UID
              }
            },
            "keys": {
              "raw": false,
              "arranged": [
                [
                  [
                    {
                      "Column": 0
                    }
                  ],
                  {
                    "0": 0
                  },
                  []
                ]
              ]
            },
            "plan": "PassArrangements"
          }
        },
        "forms": {
          "raw": true,
          "arranged": []
        },
        "input_key": [
          {
            "Column": 0
          }
        ],
        "input_mfp": {
          "expressions": [],
          "predicates": [],
          "projection": [
            0
          ],
          "input_arity": 1
        }
      }
    },
    "top_k_plan": {
      "MonotonicTopK": {
        "group_key": [],
        "order_key": [
          {
            "column": 0,
            "desc": false,
            "nulls_last": true
          }
        ],
        "limit": 2,
        "arity": 1
      }
    }
  }
}


> CREATE RECORDED VIEW r1 AS SELECT b FROM non_dbz_data

> CREATE VIEW v11 AS SELECT * FROM r1 ORDER BY b LIMIT 3

# Propagating monotonicity analysis through recorded views

? EXPLAIN PHYSICAL PLAN FOR VIEW v11
Source materialize.public.r1 UID:
{
  "predicates": [],
  "projection": [
    0
  ]
}

Query:
{
  "TopK": {
    "input": {
      "Get": {
        "id": {
          "Global": {
            UID
          }
        },
        "keys": {
          "raw": true,
          "arranged": []
        },
        "plan": "PassArrangements"
      }
    },
    "top_k_plan": {
      "MonotonicTopK": {
        "group_key": [],
        "order_key": [
          {
            "column": 0,
            "desc": false,
            "nulls_last": true
          }
        ],
        "limit": 3,
        "arity": 1
      }
    }
  }
}
