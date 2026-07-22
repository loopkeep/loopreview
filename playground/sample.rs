//! Playground fixture for review E2E testing. Never merged.

/// 挨拶を組み立てる(日本語コメントは幅テスト用・改訂版)
fn greeting(name: &str) -> String {
    format!("Hello, {name}!")
}

fn farewell(name: &str) -> String {
    format!("Goodbye, {name}. See you soon.")
}

fn whisper(text: &str) -> String {
    text.to_lowercase()
}

fn main() {
    println!("{}", greeting("world"));
    println!("{}", farewell("world"));
    println!("{}", whisper("DONE"));
    println!("ここは新しい行です");
}
