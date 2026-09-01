use toolkit_macros::ExpandVars;

#[derive(ExpandVars)]
struct Cfg {
    retries: u32,
    enabled: bool,
}

fn main() {}
