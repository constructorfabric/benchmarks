use toolkit_macros::gear;

#[gear(name="x", capabilities=[stateful], lifecycle(entry="serve", foo="bar"))]
pub struct X;

fn main() {}
