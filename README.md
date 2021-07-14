# tokio-scoped
tokio-scoped provides a `scope` function inspired by [crossbeam](https://docs.rs/crossbeam/0.8.0/crossbeam/fn.scope.html)
but for the [`tokio`](https://tokio.rs/) Runtime. A scope allows one to spawn futures which do not have a `'static` lifetime
by ensuring each future spawned in the scope completes before the scope exits.

## Example

```rust
#[tokio::main]
async fn main() {
    let mut v = String::from("Hello");
    tokio_scoped::scope(|scope| {
        // Use the scope to spawn the future.
        scope.spawn(async {
            v.push('!');
        });
    });
    // The scope won't exit until all spawned futures are complete.
    assert_eq!(v.as_str(), "Hello!");
}
```

