fn main() {
    println!("Pokemon: {} bytes", std::mem::size_of::<engine::Pokemon>());
    println!("Side:    {} bytes", std::mem::size_of::<engine::Side>());
    println!("State:   {} bytes", std::mem::size_of::<engine::State>());
}
