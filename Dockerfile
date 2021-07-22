# Copyright (c) 2017, 2020 ADLINK Technology Inc.
#
# This program and the accompanying materials are made available under the
# terms of the Eclipse Public License 2.0 which is available at
# http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
# which is available at https://www.apache.org/licenses/LICENSE-2.0.
#
# SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
#
# Contributors:
#   ADLINK zenoh team, <zenoh@adlink-labs.tech>


###
### Build part
###
FROM rust:slim as builder

WORKDIR /usr/src/zenoh-plugin-dds

# Required for correct installation of maven package
RUN mkdir /usr/share/man/man1/

# List of installed tools:
#  * for CycloneDDS
#     - g++
#     - cmake
#  * for zenoh-dds-plugin
#     - git
#     - clang
RUN apt-get update && apt-get -y install g++ cmake git clang

COPY . .
RUN cargo install --locked --path .


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

EXPOSE 7447/udp
EXPOSE 7447/tcp
EXPOSE 8000/tcp

ENV RUST_LOG info

ENTRYPOINT ["/entrypoint.sh"]
