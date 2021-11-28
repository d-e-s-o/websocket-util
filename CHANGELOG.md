0.9.0
-----
- Adjusted `test::WebSocketStream` alias to include
  `tungstenite::MaybeTlsStream`
- Switched from using `test-env-log` to `test-log`
- Bumped minimum supported Rust version to `1.51`
- Bumped `tokio-tungstenite` dependency to `0.16`


0.8.0
-----
- Bumped minimum supported Rust version to `1.45`
- Bumped `tokio-tungstenite` dependency to `0.14`


0.7.0
-----
- Replaced `async-tungstenite` dependency with `tokio-tungstenite`
- Added public `tungstenite` export
- Excluded unnecessary files from being contained in release bundle
- Bumped minimum supported Rust version to `1.43`
- Bumped `tokio` dependency to `1.0`
- Removed `tracing-futures` dependency


0.6.0
-----
- Bumped `async-tungstenite` dependency to `0.8`


0.5.0
-----
- Enabled CI pipeline comprising building, testing, linting, and
  coverage collection of the project
  - Added badges indicating pipeline status and code coverage percentage
- Bumped `async-tungstenite` dependency to `0.5`


0.4.0
-----
- Removed deserialization support
  - Removed `serde` and `serde_json` dependencies


0.3.0
-----
- Added support for sending pings and evaluating pongs
- Bumped `async-tungstenite` dependency to `0.4`


0.2.0
-----
- Switched from using `log` to `tracing` as a logging/tracing provider
- Bumped `async-tungstenite` dependency to `0.3`
- Removed `async-std` dependency


0.1.0
-----
- Initial release
