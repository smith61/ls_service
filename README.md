# Language Server Service for Rust

Implementation of an asynchronous service for creating Language Servers on top of Tokio and Futures-rs

## Example
This example reads and writes messages to the stdin and stdout streams and errors on every request with an INVALID_REQUEST error.

`Cargo.toml`:
```toml
[dependencies]
futures = "0.1"
ls_service = { git = "https://github.com/smith61/ls_service" }
lsp_rs = { git = "https://github.com/smith61/rls_proto" }
tokio-core = "0.1"
tokio-stdio = { git = "https://github.com/smith61/tokio-stdio" }
```

`main.rs`
```rust
extern crate futures;
extern crate lsp_rs;
extern crate ls_service;
extern crate tokio_core;
extern crate tokio_stdio;

use lsp_rs::{
    INVALID_REQUEST,
    ServerNotification,
    ServerRequest,
    ResponseError
};
use ls_service::{
    service
};
use tokio_core::reactor::{
    Core
};
use tokio_stdio::stdio::{
    Stdio
};

struct MessageHandler;

impl service::MessageHandler for MessageHandler {

    fn handle_request( &self, _ : service::ServiceHandle, _ : ServerRequest, output : service::ResponseOutput ) {
        output.send_error( ResponseError {
            code    : INVALID_REQUEST,
            message : "Bad request".to_string( )
        } );
    }

    fn handle_notification( &self, _ : service::ServiceHandle, _ : ServerNotification ) { }

}

fn main( ) {
    let stdio = Stdio::new( 1024, 1024 );

    let mut core = Core::new( ).unwrap( );
    let service = service::start_service( core.handle( ), MessageHandler, stdio );

    core.run( service.get_shutdown_future( ).clone( ) ).unwrap( );
}
```