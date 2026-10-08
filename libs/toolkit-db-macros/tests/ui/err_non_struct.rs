// Derive macro applied to a non-struct should abort.

use toolkit_db_macros::Scopable;

#[derive(Scopable)]
enum NotAStruct {
    A,
    B,
}

