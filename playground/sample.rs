//! Playground fixture for review E2E testing. Never merged.

/// 挨拶を組み立てる(日本語コメントは幅テスト用)
fn greeting(name: &str) -> String {
    let mut out = String::new();
    out.push_str("Hello, ");
    out.push_str(name);
    out.push_str("!");
    out
}

fn farewell(name: &str) -> String {
    format!("Goodbye, {name}.")
}

fn shout(text: &str) -> String {
    text.to_uppercase()
}

fn main() {
    println!("{}", greeting("world"));
    println!("{}", farewell("world"));
    println!("{}", shout("done"));
}
