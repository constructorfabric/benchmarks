use toolkit_macros::gear;

#[gear(name="x", capabilities=[stateful], ctor="X::new()")]
pub struct X;

fn main() {}
