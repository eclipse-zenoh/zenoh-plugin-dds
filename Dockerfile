#
# Copyright (c) 2022 ZettaScale Technology
#
# This program and the accompanying materials are made available under the
# terms of the Eclipse Public License 2.0 which is available at
# http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
# which is available at https://www.apache.org/licenses/LICENSE-2.0.
#
# SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
#
# Contributors:
#   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
#


###
### Build part
###
FROM rust:slim-buster as builder

WORKDIR /usr/src/zenoh-plugin-dds

# List of installed tools:
#  * for CycloneDDS
#     - g++
#     - cmake
#  * for zenoh-dds-plugin
#     - git
#     - clang
RUN apt-get update && apt-get -y install g++ git clang wget tar build-essential
RUN wget https://github.com/Kitware/CMake/releases/download/v3.24.2/cmake-3.24.2-linux-x86_64.tar.gz \
      -q -O /tmp/cmake-install.tar.gz \
      && tar xzf /tmp/cmake-install.tar.gz \
      && cp -r cmake-3.24.2-linux-x86_64/* /usr/ \
      && rm /tmp/cmake-install.tar.gz

RUN cmake --version

COPY . .
# if exists, copy .git directory to be used by git-version crate to determine the version
COPY .gi? .git/

RUN cargo install --locked --path zenoh-bridge-dds


###
### Run part
###
FROM debian:buster-slim

RUN apt-get update && apt-get install -y libssl1.1 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/cargo/bin/zenoh-bridge-dds /usr/local/bin/zenoh-bridge-dds
RUN ldconfig -v

RUN echo '#!/bin/bash' > /entrypoint.sh
RUN echo 'echo " * Starting: zenoh-bridge-dds $*"' >> /entrypoint.sh
RUN echo 'exec zenoh-bridge-dds $*' >> /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 7446/udp
EXPOSE 7447/tcp
EXPOSE 8000/tcp

ENV RUST_LOG info

ENTRYPOINT ["/entrypoint.sh"]
