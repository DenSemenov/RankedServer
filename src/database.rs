use postgres::{Connection, SslMode};

pub mod database {

pub fn connect() {
    let conn =
        Connection::connect(
            "postgresql://test:test@89.223.89.237:5432/rhqm",
            &SslMode::None)
        .unwrap();
        conn.execute(
            "CREATE TABLE person (
            id              SERIAL PRIMARY KEY,
            name            VARCHAR NOT NULL,
            data            BYTEA
            )",
            &[])
            .unwrap();
        }   
    }