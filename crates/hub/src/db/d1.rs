//! The Worker's [`Db`]: the `DB` D1 binding (wrangler.toml `[[d1_databases]]`).
//!
//! Integers bind as JavaScript numbers (D1's storage is SQLite; values beyond 2^53 would lose
//! precision, which no column here reaches). Nothing in this file is run natively: the native
//! tests use the in-memory fake in `tests/d1_schema.rs`, and the wasm32 build checks this code.

use worker::wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement};

use super::{Db, DbError, Param, Row};

/// A [`Db`] over one D1 database binding.
pub struct D1<'a> {
    database: &'a D1Database,
}

impl<'a> D1<'a> {
    /// Wrap a binding, e.g. `D1::new(&env.d1("DB")?)`.
    pub fn new(database: &'a D1Database) -> Self {
        Self { database }
    }

    fn prepare(&self, sql: &str, params: &[Param]) -> Result<D1PreparedStatement, DbError> {
        let values: Vec<JsValue> = params.iter().map(to_js).collect();
        self.database
            .prepare(sql)
            .bind(&values)
            .map_err(|error| DbError::Backend(error.to_string()))
    }
}

fn to_js(param: &Param) -> JsValue {
    match param {
        Param::Null => JsValue::NULL,
        Param::Int(value) => JsValue::from_f64(*value as f64),
        Param::Real(value) => JsValue::from_f64(*value),
        Param::Text(text) => JsValue::from_str(text),
    }
}

impl Db for D1<'_> {
    async fn execute(&self, sql: &str, params: &[Param]) -> Result<u64, DbError> {
        let result = self
            .prepare(sql, params)?
            .run()
            .await
            .map_err(|error| DbError::Backend(error.to_string()))?;
        let changes = result
            .meta()
            .map_err(|error| DbError::Backend(error.to_string()))?
            .and_then(|meta| meta.changes)
            .unwrap_or(0);
        Ok(u64::try_from(changes).unwrap_or(0))
    }

    async fn query(&self, sql: &str, params: &[Param]) -> Result<Vec<Row>, DbError> {
        let result = self
            .prepare(sql, params)?
            .all()
            .await
            .map_err(|error| DbError::Backend(error.to_string()))?;
        result
            .results::<Row>()
            .map_err(|error| DbError::Decode(error.to_string()))
    }
}
