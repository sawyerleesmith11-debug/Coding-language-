fn main() {
    let mut i: i64 = 0;
    let mut total: i64 = 0;
    while i < 200_000_000 {
        total = (total + i * i) % 1_000_000_007;
        i += 1;
    }
    println!("{total}");
}
