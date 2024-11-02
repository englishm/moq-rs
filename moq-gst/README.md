A gstreamer plugin utilizing [moq-rs](https://github.com/englishm/moq-rs).

# Usage
Check out the `run` script for an example pipeline.

```bash
./run
```

By default this uses a localhost relay.
You can change the ENV args if you want to make it watchable on production instead:

```bash
ADDR=relay.quic.video NAME=something ./run
```

# License

Licensed under either:

-   Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
-   MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
