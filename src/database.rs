use postgres::{Connection, SslMode};

pub mod database {

    pub fn connect() {
        let conn = Connection::connect(
            "postgresql://denis:UM5AJa3kp8@85.143.174.177:5432/minigames",
            &SslMode::None,
        )
        .unwrap();
    }}
