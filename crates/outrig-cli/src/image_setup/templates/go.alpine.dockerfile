RUN curl -fsSL https://go.dev/dl/go1.22.0.linux-amd64.tar.gz \
       | tar -C /usr/local -xz
ENV PATH=/usr/local/go/bin:$PATH
