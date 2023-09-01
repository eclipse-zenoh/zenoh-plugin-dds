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

# ROS2 package build file (waiting for an "ament_rust" built type)
cmake_minimum_required(VERSION 3.8)
project(zenoh_bridge_dds)

if(CMAKE_COMPILER_IS_GNUCXX OR CMAKE_CXX_COMPILER_ID MATCHES "Clang")
  add_compile_options(-Wall -Wextra -Wpedantic)
endif()

# find dependencies
find_package(ament_cmake REQUIRED)
# uncomment the following section in order to fill in
# further dependencies manually.
# find_package(<dependency> REQUIRED)

set(RUST_TARGET_DIR ${CMAKE_BINARY_DIR}/target)
if(DEFINED CROSS_ARCH)
  set(RUST_INSTALL_DIR ${RUST_TARGET_DIR}/${CROSS_ARCH})
  set(BUILD_CMD cargo zigbuild --release --manifest-path "${CMAKE_SOURCE_DIR}/zenoh-bridge-dds/Cargo.toml" --target-dir "${RUST_TARGET_DIR}" --target ${CROSS_ARCH})
else()
  set(RUST_INSTALL_DIR ${RUST_TARGET_DIR})
  set(BUILD_CMD cargo build --release --manifest-path "${CMAKE_SOURCE_DIR}/zenoh-bridge-dds/Cargo.toml" --target-dir "${RUST_TARGET_DIR}")
endif()

FILE(GLOB RUST_SRC_FILES
  ${CMAKE_SOURCE_DIR}/zenoh-bridge-dds/src/*.rs
  ${CMAKE_SOURCE_DIR}/zenoh-plugin-dds/src/*.rs
  ${CMAKE_SOURCE_DIR}/Cargo.lock)

add_custom_command(
    OUTPUT
        ${RUST_INSTALL_DIR}/release/zenoh-bridge-dds
    COMMAND ${BUILD_CMD}
    DEPENDS
        ${RUST_SRC_FILES}
)

add_custom_target(
    build_crate ALL
    DEPENDS
        ${RUST_INSTALL_DIR}/release/zenoh-bridge-dds
)

macro(INSTALL_ZENOH src_path dst_path)
  install(FILES
      ${src_path}
      RENAME zenoh_bridge_dds
      PERMISSIONS OWNER_READ OWNER_WRITE OWNER_EXECUTE GROUP_READ GROUP_EXECUTE WORLD_READ WORLD_EXECUTE
      DESTINATION ${dst_path}
  )
endmacro(INSTALL_ZENOH)

INSTALL_ZENOH(${RUST_INSTALL_DIR}/release/zenoh-bridge-dds lib/${PROJECT_NAME})

INSTALL_ZENOH(${RUST_INSTALL_DIR}/release/zenoh-bridge-dds bin)

ament_package()
