pub mod grammar_generator;

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;
    use rusqlite::params;

    use crate::{
        common::TempDatabase,
        fuzz::grammar_generator::{rand_int, rand_str, GrammarGenerator},
    };

    fn rng_from_time() -> (ChaCha8Rng, u64) {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let rng = ChaCha8Rng::seed_from_u64(seed);
        (rng, seed)
    }

    fn sqlite_exec_row(conn: &rusqlite::Connection, query: &str) -> Vec<rusqlite::types::Value> {
        let mut stmt = conn.prepare(&query).unwrap();
        let mut rows = stmt.query(params![]).unwrap();
        let mut columns = Vec::new();
        let row = rows.next().unwrap().unwrap();
        for i in 0.. {
            let column: rusqlite::types::Value = match row.get(i) {
                Ok(column) => column,
                Err(rusqlite::Error::InvalidColumnIndex(_)) => break,
                Err(err) => panic!("unexpected rusqlite error: {}", err),
            };
            columns.push(column);
        }
        assert!(rows.next().unwrap().is_none());

        columns
    }

    fn limbo_exec_row(
        conn: &Rc<limbo_core::Connection>,
        query: &str,
    ) -> Vec<rusqlite::types::Value> {
        let mut stmt = conn.prepare(query).unwrap();
        let result = stmt.step().unwrap();
        let row = loop {
            match result {
                limbo_core::StepResult::Row => {
                    let row = stmt.row().unwrap();
                    break row;
                }
                limbo_core::StepResult::IO => continue,
                r => panic!("unexpected result {:?}: expecting single row", r),
            }
        };
        row.values
            .iter()
            .map(|x| match x.to_value() {
                limbo_core::Value::Null => rusqlite::types::Value::Null,
                limbo_core::Value::Integer(x) => rusqlite::types::Value::Integer(x),
                limbo_core::Value::Float(x) => rusqlite::types::Value::Real(x),
                limbo_core::Value::Text(x) => rusqlite::types::Value::Text(x.to_string()),
                limbo_core::Value::Blob(x) => rusqlite::types::Value::Blob(x.to_vec()),
            })
            .collect()
    }

    #[test]
    pub fn arithmetic_expression_fuzz_ex1() {
        let db = TempDatabase::new_empty();
        let limbo_conn = db.connect_limbo();
        let sqlite_conn = rusqlite::Connection::open_in_memory().unwrap();

        for query in [
            "SELECT ~1 >> 1536",
            "SELECT ~ + 3 << - ~ (~ (8)) - + -1 - 3 >> 3 + -6 * (-7 * 9 >> - 2)",
        ] {
            let limbo = limbo_exec_row(&limbo_conn, query);
            let sqlite = sqlite_exec_row(&sqlite_conn, query);
            assert_eq!(
                limbo, sqlite,
                "query: {}, limbo: {:?}, sqlite: {:?}",
                query, limbo, sqlite
            );
        }
    }

    #[test]
    pub fn arithmetic_expression_fuzz() {
        let _ = env_logger::try_init();
        let g = GrammarGenerator::new();
        let (expr, expr_builder) = g.create_handle();
        let (bin_op, bin_op_builder) = g.create_handle();
        let (unary_op, unary_op_builder) = g.create_handle();
        let (paren, paren_builder) = g.create_handle();

        paren_builder
            .concat("")
            .push_str("(")
            .push(expr)
            .push_str(")")
            .build();

        unary_op_builder
            .concat(" ")
            .push(g.create().choice().options_str(["~", "+", "-"]).build())
            .push(expr)
            .build();

        bin_op_builder
            .concat(" ")
            .push(expr)
            .push(
                g.create()
                    .choice()
                    .options_str(["+", "-", "*", "/", "%", "&", "|", "<<", ">>"])
                    .build(),
            )
            .push(expr)
            .build();

        expr_builder
            .choice()
            .option_w(unary_op, 1.0)
            .option_w(bin_op, 1.0)
            .option_w(paren, 1.0)
            .option_symbol_w(rand_int(-10..10), 1.0)
            .build();

        let sql = g.create().concat(" ").push_str("SELECT").push(expr).build();

        let db = TempDatabase::new_empty();
        let limbo_conn = db.connect_limbo();
        let sqlite_conn = rusqlite::Connection::open_in_memory().unwrap();

        let (mut rng, seed) = rng_from_time();
        log::info!("seed: {}", seed);
        for _ in 0..1024 {
            let query = g.generate(&mut rng, sql, 50);
            let limbo = limbo_exec_row(&limbo_conn, &query);
            let sqlite = sqlite_exec_row(&sqlite_conn, &query);
            assert_eq!(
                limbo, sqlite,
                "query: {}, limbo: {:?}, sqlite: {:?}",
                query, limbo, sqlite
            );
        }
    }

    #[test]
    pub fn logical_expression_fuzz_ex1() {
        let _ = env_logger::try_init();
        let db = TempDatabase::new_empty();
        let limbo_conn = db.connect_limbo();
        let sqlite_conn = rusqlite::Connection::open_in_memory().unwrap();

        for query in [
            "SELECT FALSE",
            "SELECT NOT FALSE",
            "SELECT ((NULL) IS NOT TRUE <= ((NOT (FALSE))))",
            "SELECT ifnull(0, NOT 0)",
            "SELECT like('a%', 'a') = 1",
            "SELECT CASE ( NULL < NULL ) WHEN ( 0 ) THEN ( NULL ) ELSE ( 2.0 ) END;",
            "SELECT (COALESCE(0, COALESCE(0, 0)));",
            "SELECT CAST((1 > 0) AS INTEGER);",
        ] {
            let limbo = limbo_exec_row(&limbo_conn, query);
            let sqlite = sqlite_exec_row(&sqlite_conn, query);
            assert_eq!(
                limbo, sqlite,
                "query: {}, limbo: {:?}, sqlite: {:?}",
                query, limbo, sqlite
            );
        }
    }

    #[test]
    pub fn logical_expression_fuzz_run() {
        let _ = env_logger::try_init();
        let g = GrammarGenerator::new();
        let (expr, expr_builder) = g.create_handle();
        let (bin_op, bin_op_builder) = g.create_handle();
        let (unary_infix_op, unary_infix_op_builder) = g.create_handle();
        let (scalar, scalar_builder) = g.create_handle();
        let (paren, paren_builder) = g.create_handle();

        paren_builder
            .concat("")
            .push_str("(")
            .push(expr)
            .push_str(")")
            .build();

        unary_infix_op_builder
            .concat(" ")
            .push(g.create().choice().options_str(["NOT"]).build())
            .push(expr)
            .build();

        bin_op_builder
            .concat(" ")
            .push(expr)
            .push(
                g.create()
                    .choice()
                    .options_str(["AND", "OR", "IS", "IS NOT", "=", "<>", ">", "<", ">=", "<="])
                    .build(),
            )
            .push(expr)
            .build();

        let (like_pattern, like_pattern_builder) = g.create_handle();
        like_pattern_builder
            .choice()
            .option_str("%")
            .option_str("_")
            .option_symbol(rand_str("", 1))
            .repeat(1..10, "")
            .build();

        let (glob_pattern, glob_pattern_builder) = g.create_handle();
        glob_pattern_builder
            .choice()
            .option_str("*")
            .option_str("**")
            .option_str("A")
            .option_str("B")
            .repeat(1..10, "")
            .build();

        let (coalesce_expr, coalesce_expr_builder) = g.create_handle();
        coalesce_expr_builder
            .concat("")
            .push_str("COALESCE(")
            .push(g.create().concat("").push(expr).repeat(2..5, ",").build())
            .push_str(")")
            .build();

        let (cast_expr, cast_expr_builder) = g.create_handle();
        cast_expr_builder
            .concat(" ")
            .push_str("CAST ( (")
            .push(expr)
            .push_str(") AS ")
            // cast to INTEGER/REAL/TEXT types can be added when Limbo will use proper equality semantic between values (e.g. 1 = 1.0)
            .push(g.create().choice().options_str(["NUMERIC"]).build())
            .push_str(")")
            .build();

        let (case_expr, case_expr_builder) = g.create_handle();
        case_expr_builder
            .concat(" ")
            .push_str("CASE (")
            .push(expr)
            .push_str(")")
            .push(
                g.create()
                    .concat(" ")
                    .push_str("WHEN (")
                    .push(expr)
                    .push_str(") THEN (")
                    .push(expr)
                    .push_str(")")
                    .repeat(1..5, " ")
                    .build(),
            )
            .push_str("ELSE (")
            .push(expr)
            .push_str(") END")
            .build();

        scalar_builder
            .choice()
            .option(coalesce_expr)
            .option(
                g.create()
                    .concat("")
                    .push_str("like('")
                    .push(like_pattern)
                    .push_str("', '")
                    .push(like_pattern)
                    .push_str("')")
                    .build(),
            )
            .option(
                g.create()
                    .concat("")
                    .push_str("glob('")
                    .push(glob_pattern)
                    .push_str("', '")
                    .push(glob_pattern)
                    .push_str("')")
                    .build(),
            )
            .option(
                g.create()
                    .concat("")
                    .push_str("ifnull(")
                    .push(expr)
                    .push_str(",")
                    .push(expr)
                    .push_str(")")
                    .build(),
            )
            .option(
                g.create()
                    .concat("")
                    .push_str("iif(")
                    .push(expr)
                    .push_str(",")
                    .push(expr)
                    .push_str(",")
                    .push(expr)
                    .push_str(")")
                    .build(),
            )
            .build();

        expr_builder
            .choice()
            .option_w(cast_expr, 1.0)
            .option_w(case_expr, 1.0)
            .option_w(unary_infix_op, 2.0)
            .option_w(bin_op, 3.0)
            .option_w(paren, 2.0)
            .option_w(scalar, 4.0)
            // unfortunatelly, sqlite behaves weirdly when IS operator is used with TRUE/FALSE constants
            // e.g. 8 IS TRUE == 1 (although 8 = TRUE == 0)
            // so, we do not use TRUE/FALSE constants as they will produce diff with sqlite results
            .options_str(["1", "0", "NULL", "2.0", "1.5", "-0.5", "-2.0", "(1 / 0)"])
            .build();

        let sql = g.create().concat(" ").push_str("SELECT").push(expr).build();

        let db = TempDatabase::new_empty();
        let limbo_conn = db.connect_limbo();
        let sqlite_conn = rusqlite::Connection::open_in_memory().unwrap();

        let (mut rng, seed) = rng_from_time();
        log::info!("seed: {}", seed);
        for _ in 0..1024 {
            let query = g.generate(&mut rng, sql, 50);
            log::info!("query: {}", query);
            let limbo = limbo_exec_row(&limbo_conn, &query);
            let sqlite = sqlite_exec_row(&sqlite_conn, &query);
            assert_eq!(
                limbo, sqlite,
                "query: {}, limbo: {:?}, sqlite: {:?}",
                query, limbo, sqlite
            );
        }
    }
}
