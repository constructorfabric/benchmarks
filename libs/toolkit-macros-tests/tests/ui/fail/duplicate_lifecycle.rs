use toolkit_macros::gear;

#[gear(name="x", capabilities=[stateful], lifecycle(entry="serve"), lifecycle(entry="serve"))]
pub struct X;

fn main() {}
