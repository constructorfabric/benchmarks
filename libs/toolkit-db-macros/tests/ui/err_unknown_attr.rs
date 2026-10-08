// Unknown attribute key should abort with a clear message.

use toolkit_db_macros::Scopable;

#[derive(Scopable)]
#[secure(does_not_exist = "oops")]
struct Model;

