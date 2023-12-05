use once_cell::sync::OnceCell;
use chat_core::{Database, DatabaseInfo};
use std::env;

static DATABASE_CONNECTION: OnceCell<Database> = OnceCell::new();

pub async fn connect() {
    let database = DatabaseInfo::MongoDb(env::var("MONGODB").unwrap())
        .connect()
        .await
        .expect("Failed to connect to the database.");

    DATABASE_CONNECTION
        .set(database)
        .expect("Setting `Database`");
}

/// Get a reference to the current database.
pub fn get_db() -> &'static Database {
    DATABASE_CONNECTION.get().expect("Valid `Database`")
}
