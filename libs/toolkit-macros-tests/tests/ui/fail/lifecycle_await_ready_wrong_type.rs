use toolkit_macros::gear;

#[gear(name="x", capabilities=[stateful], lifecycle(entry="serve", await_ready="true"))]
pub struct X;

fn main() {}
