<img src="https://raw.githubusercontent.com/eclipse-zenoh/zenoh/main/zenoh-dragon.png" height="150">

<!--- 
[![CI](https://github.com/eclipse-zenoh/zenoh-plugin-dds/workflows/Rust/badge.svg)](https://github.com/eclipse-zenoh/zenoh-plugin-dds/actions?query=workflow%3ARust)
--->
[![Discussion](https://img.shields.io/badge/discussion-on%20github-blue)](https://github.com/eclipse-zenoh/roadmap/discussions)
[![Discord](https://img.shields.io/badge/chat-on%20discord-blue)](https://discord.gg/2GJ958VuHs)
[![License](https://img.shields.io/badge/License-EPL%202.0-blue)](https://choosealicense.com/licenses/epl-2.0/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

# Eclipse Zenoh
The Eclipse Zenoh: Zero Overhead Pub/sub, Store/Query and Compute.

Zenoh (pronounce _/zeno/_) unifies data in motion, data at rest and computations. It carefully blends traditional pub/sub with geo-distributed storages, queries and computations, while retaining a level of time and space efficiency that is well beyond any of the mainstream stacks.

Check the website [zenoh.io](http://zenoh.io) and the [roadmap](https://github.com/eclipse-zenoh/roadmap) for more detailed information.

-------------------------------
# DDS plugin and standalone `zenoh-bridge-dds`

:point_right: **Install latest release:** see [below](#How-to-install-it)

:point_right: **Docker image:** see [below](#Docker-image)

:point_right: **Build "main" branch:** see [below](#How-to-build-it)

## Background
The Data Distribution Service (DDS) is a standard for data-centric publish subscribe. Whilst DDS has been around for quite some time and has a long history of deployments in various industries, it has recently gained quite a bit of attentions thanks to its adoption by the Robotic Operating System (ROS 2) -- where it is used for communication between ROS 2 nodes.

### ⚠️ On usage with ROS 2 ⚠️ 

This plugin is based on the DDS standard, and thus can work with ROS 2 to some extent.

However we strongly advise ROS 2 users to rather try the **new [`zenoh-plugin-ros2dds`](https://github.com/eclipse-zenoh/zenoh-plugin-ros2dds)** which is dedicated to the support of ROS 2 with DDS.
Thanks to a better integration with ROS 2 concepts, this new plugin comes with those benefits:

- Better integration of the **ROS graph** (all ROS topics/services/actions can be seen across bridges)
- Better support of **ROS toolings** (ros2 CLI, rviz2...)
- Configuration of a **ROS namespace** on the bridge (instead of on each ROS Node)
- Services and Action as **Zenoh Queryables** with more efficiency and scalability that RPC over DDS
- Even more **compact discovery information** between the bridges (not forwarding all `ros_discovery_info` messages as such)

This Zenoh plugin for DDS will eventually be deprecated for ROS 2 usage.

## Plugin or bridge ?

This software is built in 2 ways to choose from:
 - `zenoh-plugin-dds`: a Zenoh plugin - a dynamic library that can be loaded by a Zenoh router
 - `zenoh-bridge-dds`: a standalone executable

The features and configurations descibed in this document applies to both.
Meaning the *"plugin"* and *"bridge"*  words are interchangeables in the rest of this document.

## How to install it

To install the latest release of either the DDS plugin for the Zenoh router, either the `zenoh-bridge-dds` standalone executable, you can do as follows:

### Manual installation (all platforms)

All release packages can be downloaded from:  
 - https://download.eclipse.org/zenoh/zenoh-plugin-dds/latest/   

Each subdirectory has the name of the Rust target. See the platforms each target corresponds to on https://doc.rust-lang.org/stable/rustc/platform-support.html

Choose your platform and download:
 - the `zenoh-plugin-dds-<version>-<platform>.zip` file for the plugin.  
   Then unzip it in the same directory than `zenohd` or to any directory where it can find the plugin library (e.g. /usr/lib)
 - the `zenoh-bridge-dds-<version>-<platform>.zip` file for the standalone executable.  
   Then unzip it where you want, and run the extracted `zenoh-bridge-dds` binary.

### Linux Debian

Add Eclipse Zenoh private repository to the sources list:

```bash
echo "deb [trusted=yes] https://download.eclipse.org/zenoh/debian-repo/ /" | sudo tee -a /etc/apt/sources.list > /dev/null
sudo apt update
```
Then either:
  - install the plugin with: `sudo apt install zenoh-plugin-dds`.
  - install the standalone executable with: `sudo apt install zenoh-bridge-dds`.

## How to build it

> :warning: **WARNING** :warning: : Zenoh and its ecosystem are under active development. When you build from git, make sure you also build from git any other Zenoh repository you plan to use (e.g. binding, plugin, backend, etc.). It may happen that some changes in git are not compatible with the most recent packaged Zenoh release (e.g. deb, docker, pip). We put particular effort in mantaining compatibility between the various git repositories in the Zenoh project.

> :warning: **WARNING** :warning: : As Rust doesn't have a stable ABI, the plugins should be
built with the exact same Rust version than `zenohd`, and using for `zenoh` dependency the same version (or commit number) than 'zenohd'.
Otherwise, incompatibilities in memory mapping of shared types between `zenohd` and the library can lead to a `"SIGSEV"` crash.

In order to build the zenoh bridge for DDS you need first to install the following dependencies:

- [Rust](https://www.rust-lang.org/tools/install). If you already have the Rust toolchain installed, make sure it is up-to-date with:

  ```bash
  $ rustup update
  ```

- On Linux, make sure the `llvm` and `clang` development packages are installed:
   - on Debians do: `sudo apt install llvm-dev libclang-dev`
   - on CentOS or RHEL do: `sudo yum install llvm-devel clang-devel`
   - on Alpine do: `apk install llvm11-dev clang-dev`
- [CMake](https://cmake.org/download/) (to build CycloneDDS which is a native dependency)

Once these dependencies are in place, you may clone the repository on your machine:

```bash
$ git clone https://github.com/eclipse-zenoh/zenoh-plugin-dds.git
$ cd zenoh-plugin-dds
```

You can then choose between building the zenoh bridge for DDS:
- as a plugin library that can be dynamically loaded by the zenoh router (`zenohd`):
```bash
$ cargo build --release -p zenoh-plugin-dds
```
The plugin shared library (`*.so` on Linux, `*.dylib` on Mac OS, `*.dll` on Windows) will be generated in the `target/release` subdirectory.

- or as a standalone executable binary:
```bash
$ cargo build --release -p zenoh-bridge-dds
```
The **`zenoh-bridge-dds`** binary will be generated in the `target/release` sub-directory.


### Enabling Cyclone DDS Shared Memory Support

Cyclone DDS Shared memory support is provided by the [Iceoryx library](https://iceoryx.io/). Iceoryx introduces additional system requirements which are documented [here](https://iceoryx.io/v2.0.1/getting-started/installation/#dependencies).

To build the zenoh bridge for DDS with support for shared memory the `dds_shm` optional feature must be enabled during the build process as follows:
- plugin library:
```bash
$ cargo build --release -p zenoh-plugin-dds --features dds_shm
```

- standalone executable binary:
```bash
$ cargo build --release -p zenoh-bridge-dds --features dds_shm
```

**Note:** Iceoryx does not need to be installed to build the bridge when the `dds_shm` feature is enabled. Iceoryx will be automatically downloaded, compiled, and statically linked into the zenoh bridge as part of the cargo build process.

When the zenoh bridge is configured to use DDS shared memory (see [Configuration](#configuration)) the **Iceoryx RouDi daemon (`iox-roudi`)** must be running in order for the bridge to start successfully. If not started the bridge will wait for a period of time for the daemon to become available before timing out and terminating.

When building the zenoh bridge with the `dds_shm` feature enabled the `iox-roudi` daemon is also built for convenience. The daemon can be found under `target/debug|release/build/cyclors-<hash>/out/iceoryx-build/bin/iox-roudi`.

See [here](https://cyclonedds.io/docs/cyclonedds/latest/shared_memory/shared_memory.html) for more details of shared memory support in Cyclone DDS.


## ROS 2 package
:warning: **Please consider using [`zenoh-bridge-ros2dds`](https://github.com/eclipse-zenoh/zenoh-plugin-ros2dds) which is dedicated to ROS 2.**

If you're a ROS 2 user, you can also build `zenoh-bridge-dds` as a ROS package running:
```bash
rosdep install --from-paths . --ignore-src -r -y
colcon build --packages-select zenoh_bridge_dds --cmake-args -DCMAKE_BUILD_TYPE=Release
```
The `rosdep` command will automatically install *Rust* and *clang* as build dependencies.

If you want to cross-compile the package on x86 device for any target, you can use the following command:
```bash
rosdep install --from-paths . --ignore-src -r -y
colcon build --packages-select zenoh_bridge_dds --cmake-args -DCMAKE_BUILD_TYPE=Release  --cmake-args -DCROSS_ARCH=<target>
```
where `<target>` is the target architecture (e.g. `aarch64-unknown-linux-gnu`). The architechture list can be found [here](https://doc.rust-lang.org/nightly/rustc/platform-support.html).

The cross-compilation uses `zig` as a linker. You can install it with instructions in [here](https://ziglang.org/download/). Also, the `zigbuild` package is required to be installed on the target device. You can install it with instructions in [here](https://github.com/rust-cross/cargo-zigbuild#installation).

## Docker image
The **`zenoh-bridge-dds`** standalone executable is also available as a [Docker images](https://hub.docker.com/r/eclipse/zenoh-bridge-dds/tags?page=1&ordering=last_updated) for both amd64 and arm64. To get it, do:
  - `docker pull eclipse/zenoh-bridge-dds:latest` for the latest release
  - `docker pull eclipse/zenoh-bridge-dds:main` for the main branch version (nightly build)

:warning: **However, notice that it's usage is limited to Docker on Linux and using the `--net host` option.**  
The cause being that DDS uses UDP multicast and Docker doesn't support UDP multicast between a container and its host (see cases [moby/moby#23659](https://github.com/moby/moby/issues/23659), [moby/libnetwork#2397](https://github.com/moby/libnetwork/issues/2397) or [moby/libnetwork#552](https://github.com/moby/libnetwork/issues/552)). The only known way to make it work is to use the `--net host` option that is [only supported on Linux hosts](https://docs.docker.com/network/host/).

Usage: **`docker run --init --net host eclipse/zenoh-bridge-dds`**  
It supports the same command line arguments than the `zenoh-bridge-dds` (see below or check with `-h` argument).

-------------------------------
# Usage

The use cases of this Zenoh plugin for DDS are various:
- integration of a DDS System with a Zenoh System
- communication between DDS System and embeded devices thanks to [zenoh-pico](https://github.com/eclipse-zenoh/zenoh-pico)
- bridging between different DDS Systems, across various transports, via a Zenoh infrastructure (i.e. some routers or directly in peer-to-peer between the bridges)
- scaling a DDS system up to the Cloud with Zenoh routers
- integration with any technology supported by other Zenoh Plugins (MQTT, ROS 2 ...) or Storages technology (InfluxDB, RocksDB)


## Configuration

`zenoh-bridge-dds` can be configured via a JSON5 file passed via the `-c`argument. You can see a commented example of such configuration file: [`DEFAULT_CONFIG.json5`](DEFAULT_CONFIG.json5).

The `"dds"` part of this same configuration file can also be used in the configuration file for the zenoh router (within its `"plugins"` part). The router will automatically try to load the plugin library (`zenoh-plugin_dds`) at startup and apply its configuration.

`zenoh-bridge-dds` also accepts the following arguments. If set, each argument will override the similar setting from the configuration file:
 * zenoh-related arguments:
   - **`-c, --config <FILE>`** : a config file
   - **`-m, --mode <MODE>`** : The zenoh session mode. Default: `peer` Possible values: `peer` or `client`.  
      See [zenoh documentation](https://zenoh.io/docs/getting-started/key-concepts/#deployment-units) for more details.
   - **`-l, --listen <LOCATOR>`** : A locator on which this router will listen for incoming sessions. Repeat this option to open several listeners. Example of locator: `tcp/localhost:7447`.
   - **`-e, --peer <LOCATOR>`** : A peer locator this router will try to connect to (typically another bridge or a zenoh router). Repeat this option to connect to several peers. Example of locator: `tcp/<ip-address>:7447`.
   - **`--no-multicast-scouting`** : disable the zenoh scouting protocol that allows automatic discovery of zenoh peers and routers.
   - **`-i, --id <hex_string>`** : The identifier (as an hexadecimal string - e.g.: 0A0B23...) that the zenoh bridge must use. **WARNING: this identifier must be unique in the system!** If not set, a random UUIDv4 will be used.
   - **`--group-member-id <ID>`** : The bridges are supervising each other via zenoh liveliness tokens. This option allows to set a custom identifier for the bridge, that will be used the liveliness token key (if not specified, the zenoh UUID is used).
   - **`--rest-http-port <rest-http-port>`** : set the REST API http port (default: 8000)
 * DDS-related arguments:
   - **`-d, --domain <ID>`** : The DDS Domain ID. By default set to `0`, or to `"$ROS_DOMAIN_ID"` is this environment variable is defined.
   - **`--dds-localhost-only`** : If set, the DDS discovery and traffic will occur only on the localhost interface (127.0.0.1).
     By default set to false, unless the "ROS_LOCALHOST_ONLY=1" environment variable is defined.
   - **`--dds-enable-shm`** : If set, DDS will be configured to use shared memory. Requires the bridge to be built with the 'dds_shm' feature for this option to valid.
     By default set to false.
   - **`-f, --fwd-discovery`** : When set, rather than creating a local route when discovering a local DDS entity, this discovery info is forwarded to the remote plugins/bridges. Those will create the routes, including a replica of the discovered entity. More details [here](#full-support-of-ros-graph-and-topic-lists-via-the-forward-discovery-mode)
   - **`-s, --scope <String>`** : A string used as prefix to scope DDS traffic when mapped to zenoh keys.
   - **`-a, --allow <String>`** :  A regular expression matching the set of 'partition/topic-name' that must be routed via zenoh.
     By default, all partitions and topics are allowed.  
     If both 'allow' and 'deny' are set a partition and/or topic will be allowed if it matches only the 'allow' expression.  
     Repeat this option to configure several topic expressions. These expressions are concatenated with '|'.
     Examples of expressions: 
        - `.*/TopicA` will allow only the `TopicA` to be routed, whatever the partition.
        - `PartitionX/.*` will allow all the topics to be routed, but only on `PartitionX`.
        - `cmd_vel|rosout` will allow only the topics containing `cmd_vel` or `rosout` in their name or partition name to be routed.
   - **`--deny <String>`** :  A regular expression matching the set of 'partition/topic-name' that must NOT be routed via zenoh.
     By default, no partitions and no topics are denied.  
     If both 'allow' and 'deny' are set a partition and/or topic will be allowed if it matches only the 'allow' expression.  
     Repeat this option to configure several topic expressions. These expressions are concatenated with '|'.
   - **`--max-frequency <String>...`** : specifies a maximum frequency of data routing over zenoh per-topic. The string must have the format `"regex=float"` where:
       - `"regex"` is a regular expression matching the set of 'partition/topic-name' for which the data (per DDS instance) must be routedat no higher rate than associated max frequency (same syntax than --allow option).
       - `"float"` is the maximum frequency in Hertz; if publication rate is higher, downsampling will occur when routing.

       (usable multiple times)
   - **`--queries-timeout <Duration>`**: A duration in seconds (default: 5.0 sec) that will be used as a timeout when the bridge
     queries any other remote bridge for discovery information and for historical data for TRANSIENT_LOCAL DDS Readers it serves
     (i.e. if the query to the remote bridge exceed the timeout, some historical samples might be not routed to the Readers,
     but the route will not be blocked forever).
   - **`-w, --generalise-pub <String>`** :  A list of key expressions to use for generalising the declaration of
     the zenoh publications, and thus minimizing the discovery traffic (usable multiple times).
     See [this blog](https://zenoh.io/blog/2021-03-23-discovery/#leveraging-resource-generalisation) for more details.
   - **`-r, --generalise-sub <String>`** :  A list of key expressions to use for generalising the declaration of
     the zenoh subscriptions, and thus minimizing the discovery traffic (usable multiple times).
     See [this blog](https://zenoh.io/blog/2021-03-23-discovery/#leveraging-resource-generalisation) for more details.

## Admin space

The zenoh bridge for DDS exposes an administration space allowing to browse the DDS entities that have been discovered (with their QoS), and the routes that have been established between DDS and zenoh.
This administration space is accessible via any zenoh API, including the REST API that you can activate at `zenoh-bridge-dds` startup using the `--rest-http-port` argument.

The `zenoh-bridge-dds` exposes this administration space with paths prefixed by `@/service/<uuid>/dds` (where `<uuid>` is the unique identifier of the bridge instance). The informations are then organized with such paths:
 - `@/service/<uuid>/dds/version` : the bridge version
 - `@/service/<uuid>/dds/config` : the bridge configuration
 - `@/service/<uuid>/dds/participant/<gid>/reader/<gid>/<topic>` : a discovered DDS reader on `<topic>`
 - `@/service/<uuid>/dds/participant/<gid>/writer/<gid>/<topic>` : a discovered DDS reader on `<topic>`
 - `@/service/<uuid>/dds/route/from_dds/<zenoh-resource>` : a route established from a DDS writer to a zenoh key named `<zenoh-resource>` (see [mapping rules](#mapping-dds-topics-to-zenoh-resources)).
 - `@/service/<uuid>/dds/route/to_dds/<zenoh-resource>` : a route established from a zenoh key named `<zenoh-resource>` (see [mapping rules](#mapping-dds-topics-to-zenoh-resources))..

Example of queries on administration space using the REST API with the `curl` command line tool (don't forget to activate the REST API with `--rest-http-port 8000` argument):
 - List all the DDS entities that have been discovered:
    ```bash
    curl http://localhost:8000/@/service/**/participant/**
    ```
 - List all established routes:
    ```bash
    curl http://localhost:8000/@/service/**/route/**
    ```
 - List all discovered DDS entities and established route for topic `cmd_vel`:
    ```bash
    curl http://localhost:8000/@/service/**/cmd_vel
    ```

> _Pro tip: pipe the result into [**jq**](https://stedolan.github.io/jq/) command for JSON pretty print or transformation._

## Architecture details

The **zenoh bridge for DDS** discovers all DDS Writers and Readers in a DDS system and routes each DDS publication on a topic `T` as a Zenoh publication on key expression `T`. In the other way, assuming a DDS Reader on topic `T` is discovered, it routes each Zenoh publication on key expression `T` as a DDS publication on topic `T`.

The bridge doesn't deserialize the DDS data which are received from DDS Writer as a `SerializedPayload` with the representation defined in §10 of the [DDS-RTPS specification](https://www.omg.org/spec/DDSI-RTPS/2.5/PDF). Therefore, the payload published from any Zenoh application for a DDS Reader served by the bridge must have such format:
```
 0...2...........8...............16..............24..............32
 +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
 |   representation_identifier   |    representation_options     |
 +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
 ~                                                               ~
 ~ ... Bytes of data representation using a format that ...      ~
 ~ ... depends on the RepresentationIdentifier and options ...   ~
 ~                                                               ~
 +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```
Where the first 4 bytes (representation_identifier and representation_options) are usually `{0x00, 0x0}` for Big Endian encoding or `{0x00, 0x01}` for Little Endian encoding, and the remaining bytes are the data encoded in CDR.


In details, whether it's built as a library or as a standalone executable, it does the same things:
- in default mode:
  - it discovers the DDS readers and writers declared by any DDS application, via the standard DDS discovery protocol (that uses UDP multicast)
  - it creates a mirror DDS writer or reader for each discovered reader or writer (using the same QoS)
  - if maps the discovered DDS topics and partitions to zenoh keys (see mapping details below)
  - it forwards user's data from a DDS topic to the corresponding zenoh key, and vice versa
  - it does not forward to the remote bridge any DDS discovery information

- in "forward discovery" mode
  - each bridge will forward via zenoh the local DDS discovery data to the remote bridges (in a more compact way than the original DDS discovery traffic)
  - each bridge receiving DDS discovery data via zenoh will create a replica of the DDS reader or writer, with similar QoS. Those replicas will serve the route to/from zenoh, and will be discovered by the ROS2 nodes.
  - for ROS 2 systems, each bridge will forward the `ros_discovery_info` data (in a less intensive way than the original publications) to the remote bridges. On reception, the remote bridges will convert the original entities' GIDs into the GIDs of the corresponding replicas, and re-publish on DDS the `ros_discovery_info`. The full ROS graph can then be discovered by the ROS 2 nodes on each host.


### _Mapping of DDS topics to zenoh keys_
The mapping between DDS and zenoh is rather straightforward: given a DDS Reader/Writer for topic **`A`** without the partition QoS set, then the equivalent zenoh key will have the same name: **`A`**.
If a partition QoS **`P`** is defined, the equivalent zenoh key will be named as **`P/A`**.

Optionally, the bridge can be configured with a **scope** that will be used as a prefix to each zenoh key.
That is, for scope **`S`** the equivalent zenoh key will be:
 - **`S/A`** for a topic **`A`** without partition
 - **`S/P/A`** for a topic **`A`** and a partition **`P`**

### _Mapping ROS 2 names to zenoh keys_
The mapping from ROS 2 topics and services name to DDS topics is specified [here](https://design.ros2.org/articles/topic_and_service_names.html#mapping-of-ros-2-topic-and-service-names-to-dds-concepts).
Notice that ROS 2 does not use the DDS partitions.  
As a consequence of this mapping and of the DDS to zenoh mapping specified above, here are some examples of mapping from ROS 2 names to zenoh keys:

| ROS2 names | DDS Topics names | zenoh keys (no scope) | zenoh keys (if scope="`myscope`") |
| --- | --- | --- | --- |
| topic: `/rosout` | `rt/rosout` | `rt/rosout` | `myscope/rt/rosout` |
| topic: `/turtle1/cmd_vel` | `rt/turtle1/cmd_vel` | `rt/turtle1/cmd_vel` | `myscope/rt/turtle1/cmd_vel` |
| service: `/turtle1/set_pen` | `rq/turtle1/set_penRequest`<br>`rr/turtle1/set_penReply` | `rq/turtle1/set_penRequest`<br>`rr/turtle1/set_penReply` | `myscope/rq/turtle1/set_penRequest`<br>`myscope/rr/turtle1/set_penReply` |
| action: `/turtle1/rotate_absolute` | `rq/turtle1/rotate_absolute/_action/send_goalRequest`<br>`rr/turtle1/rotate_absolute/_action/send_goalReply`<br>`rq/turtle1/rotate_absolute/_action/cancel_goalRequest`<br>`rr/turtle1/rotate_absolute/_action/cancel_goalReply`<br>`rq/turtle1/rotate_absolute/_action/get_resultRequest`<br>`rr/turtle1/rotate_absolute/_action/get_resultReply`<br>`rt/turtle1/rotate_absolute/_action/status`<br>`rt/turtle1/rotate_absolute/_action/feedback` | `rq/turtle1/rotate_absolute/_action/send_goalRequest`<br>`rr/turtle1/rotate_absolute/_action/send_goalReply`<br>`rq/turtle1/rotate_absolute/_action/cancel_goalRequest`<br>`rr/turtle1/rotate_absolute/_action/cancel_goalReply`<br>`rq/turtle1/rotate_absolute/_action/get_resultRequest`<br>`rr/turtle1/rotate_absolute/_action/get_resultReply`<br>`rt/turtle1/rotate_absolute/_action/status`<br>`rt/turtle1/rotate_absolute/_action/feedback` | `myscope/rq/turtle1/rotate_absolute/_action/send_goalRequest`<br>`myscope/rr/turtle1/rotate_absolute/_action/send_goalReply`<br>`myscope/rq/turtle1/rotate_absolute/_action/cancel_goalRequest`<br>`myscope/rr/turtle1/rotate_absolute/_action/cancel_goalReply`<br>`myscope/rq/turtle1/rotate_absolute/_action/get_resultRequest`<br>`myscope/rr/turtle1/rotate_absolute/_action/get_resultReply`<br>`myscope/rt/turtle1/rotate_absolute/_action/status`<br>`myscope/rt/turtle1/rotate_absolute/_action/feedback` |
| all parameters for node `turtlesim`| `rq/turtlesim/list_parametersRequest`<br>`rr/turtlesim/list_parametersReply`<br>`rq/turtlesim/describe_parametersRequest`<br>`rr/turtlesim/describe_parametersReply`<br>`rq/turtlesim/get_parametersRequest`<br>`rr/turtlesim/get_parametersReply`<br>`rr/turtlesim/get_parameter_typesReply`<br>`rq/turtlesim/get_parameter_typesRequest`<br>`rq/turtlesim/set_parametersRequest`<br>`rr/turtlesim/set_parametersReply`<br>`rq/turtlesim/set_parameters_atomicallyRequest`<br>`rr/turtlesim/set_parameters_atomicallyReply` | `rq/turtlesim/list_parametersRequest`<br>`rr/turtlesim/list_parametersReply`<br>`rq/turtlesim/describe_parametersRequest`<br>`rr/turtlesim/describe_parametersReply`<br>`rq/turtlesim/get_parametersRequest`<br>`rr/turtlesim/get_parametersReply`<br>`rr/turtlesim/get_parameter_typesReply`<br>`rq/turtlesim/get_parameter_typesRequest`<br>`rq/turtlesim/set_parametersRequest`<br>`rr/turtlesim/set_parametersReply`<br>`rq/turtlesim/set_parameters_atomicallyRequest`<br>`rr/turtlesim/set_parameters_atomicallyReply` | `myscope/rq/turtlesim/list_parametersRequest`<br>`myscope/rr/turtlesim/list_parametersReply`<br>`myscope/rq/turtlesim/describe_parametersRequest`<br>`myscope/rr/turtlesim/describe_parametersReply`<br>`myscope/rq/turtlesim/get_parametersRequest`<br>`myscope/rr/turtlesim/get_parametersReply`<br>`myscope/rr/turtlesim/get_parameter_typesReply`<br>`myscope/rq/turtlesim/get_parameter_typesRequest`<br>`myscope/rq/turtlesim/set_parametersRequest`<br>`myscope/rr/turtlesim/set_parametersReply`<br>`myscope/rq/turtlesim/set_parameters_atomicallyRequest`<br>`myscope/rr/turtlesim/set_parameters_atomicallyReply` |
| specific ROS discovery topic | `ros_discovery_info` | `ros_discovery_info` | `myscope/ros_discovery_info`
