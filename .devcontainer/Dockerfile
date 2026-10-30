FROM ubuntu:latest@sha256:b7f48194d4d8b763a478a621cdc81c27be222ba2206ca3ca6bc42b49685f3d9e

# Vulkan for the generator's search: the loader, the Mesa software driver (lavapipe, which
# the search uses when no hardware device is present), diagnostics (`vulkaninfo --summary`),
# and the GLSL compiler that builds the compute shaders.
RUN apt update && export DEBIAN_FRONTEND=noninteractive && apt -y install --no-install-recommends \
    npm \
    curl \
    perl \
    gpg \
    protobuf-compiler \
    build-essential \
    libvulkan1 \
    mesa-vulkan-drivers \
    vulkan-tools \
    glslc

COPY entrypoint.sh /home/ubuntu/entrypoint.sh
RUN chmod +x /home/ubuntu/entrypoint.sh

USER ubuntu

RUN cd $HOME && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
RUN cd $HOME && . $HOME/.cargo/env && rustup toolchain install stable --component rust-src
RUN cd $HOME && . $HOME/.cargo/env && cargo install cargo-make cargo-espflash
RUN cd $HOME && . $HOME/.cargo/env && cargo install espup --locked
RUN cd $HOME && . $HOME/.cargo/env && rustup target add riscv32imac-unknown-none-elf
RUN cd $HOME && . $HOME/.cargo/env && rustup target add aarch64-unknown-linux-musl

ENTRYPOINT [ "/home/ubuntu/entrypoint.sh" ]
CMD [ "/bin/bash" ]

