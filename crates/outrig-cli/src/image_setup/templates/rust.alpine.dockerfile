ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
       | sh -s -- -y --no-modify-path --default-toolchain stable \
                  --profile minimal --component rustfmt,clippy \
 && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME"
