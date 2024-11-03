# rust-tls-duplex-wrapper
Full duplex stream wrapper around rust-tls

## Motivation
The Stream wrappers from rust-tls by default are not full duplex.
Reads/Writes will block each other. 
While it's not possible with rust tls to achieve true full duplex,
this crate will at least get rid of the io half duplex limit by delegating the actual io to a background thread
and ensuring that a read that causes a write (of tls control data) 
will get priority over an ordinary write and vice verse.

## Caveats
I do not care about security, only performance and ease of use.
The programs I make operates in an environment that is secure (mostly air gapped) and the
only reason why I even need tls in the first place is because of compliance requirements set by technically illiterate people.
It is my opinion that the applications I make would be just as secure if I used plaintext communication.

If your use-case actually requires "true" security then please audit this crate before using it. 
I lack the skills required to do so, and it's not required for my use cases. It's however
extremely unlikely that this crate will hold up under any such scrutiny.

As should be obvious with anything with version 0.1.0, this crate is still relatively experimental.

Only use this crate if you accept these caveats.

## Example
```rust
#[test]
fn main_client() {
    let client_config: rusttls::ClientConfig = ...;
    let config = Arc::new(client_config);
    
    let socket = std::net::TcpStream::connect("browserleaks.com:443").expect("failed to connect");

    let dns_name = ServerName::try_from("browserleaks.com").expect("invalid DNS name");
    let client = rustls::ClientConnection::new(config, dns_name).unwrap();
    let wrapper = RustTlsDuplexStream::new_unpooled(client, socket.try_clone().unwrap(), socket).expect("error spawning threads");
    wrapper.write(b"GET / HTTP/1.1\r\nHost: browserleaks.com\r\nConnection: close\r\n\r\n").expect("Error writing to stream");
    wrapper.flush().expect("Error flushing stream");
    let mut plaintext = Vec::new();
    wrapper.read_to_end(&mut plaintext).unwrap();
    
    // Will print some nice html to stdout.
    io::stdout().write_all(&plaintext).unwrap();
}

```