use postgres::{Connection, SslMode};

pub mod database {

    pub fn connect() {
        let conn = Connection::connect(
            "postgresql://test:test@85.143.174.177:5432/rhqm",
            &SslMode::None,
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE person (
            id              SERIAL PRIMARY KEY,
            name            VARCHAR NOT NULL,
            data            BYTEA
            )",
            &[],
        )
        .unwrap();
    }}
