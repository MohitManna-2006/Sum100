// Temporary: the Book::seq field and the Polymarket fee model are defined now
// but not consumed until a later phase wires up live venue feeds. Remove this
// allow once those are in use.
#![allow(dead_code)]

mod fees;
mod solver;
mod types;

fn main() {
    println!("sum100");
}
