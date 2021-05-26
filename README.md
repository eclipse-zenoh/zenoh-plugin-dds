![zenoh banner](http://zenoh.io/img/zenoh-dragon-small.png)

[![CI](https://github.com/eclipse-zenoh/zenoh-plugin-dds/workflows/Rust/badge.svg)](https://github.com/eclipse-zenoh/zenoh-plugin-dds/actions?query=workflow%3ARust)
[![License](https://img.shields.io/badge/License-EPL%202.0-blue)](https://choosealicense.com/licenses/epl-2.0/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

# DDS plugin for Eclipse zenoh

## Background
The Data Distribution Service (DDS) is a standard for data-centric publish subscribe. Whilst DDS has been around for quite some time and has a long history of deployments in various industries, it has recently gained quite a bit of attentions thanks to its adoption by the Robotic Operating System (ROS2) -- where it is used for communication between ROS2 nodes.

## Robot Swarms and Edge Robotics
As mentioned above, ROS2 has adopted DDS as the mechanism to exchange data between nodes within and potentially across a robot. That said, due to some of the very core assumptions at the foundations of the DDS wire-protocol, beside the fact that it leverages UDP/IP multicast for communication, it is not so straightforward to scale DDS communication over a WAN or across multiple LANs. Zenoh, on the other hand was designed since its inception to operate at Internet Scale.

![zenoh-plugin-dds](http://zenoh.io/img/wiki/zenoh-plugin-dds.png)

Thus, the main motivations to have a **zenoh bridge** for **DDS** are:

- Facilitate the interconnection of robot swarms.
- Support use cases of edge robotics.
- Give the possibility to use **zenoh**'s geo-distributed storage and query system to better manage robot's data.

## Architecture

The **zenoh bridge for DDS** will soon be available as a library that can be loaded by a zenoh router at startup.

Currently, it's a standalone executable named **`zenoh-bridge-dds`** that:
- discover the DDS readers and writers declared by any DDS application, via the standard DDS discovery protocol (that uses UDP multicast)
- create a mirror DDS writer or reader for each discovered reader or writer (using the same QoS)
- map the discovered DDS topics and partitions to zenoh resources (see mapping details below)
- forward user's data from a DDS topic to the corresponding zenoh resource, and vice versa

### Routing of DDS discovery information
:warning: **Important notice** :warning: :  
The DDS discovery protocol is not routed through zenoh.  
Meaning that, in case you use 2 **`zenoh-bridge-dds`** to interconnect 2 DDS domains, the DDS entities discovered in one domain won't be advertised in the other domain. Thus, the DDS data will be routed between the 2 domains only if matching readers and writers are declared in the 2 domains independently.

### Mapping DDS topics to zenoh resources
The mapping between DDS and zenoh is rather straightforward. Given a DDS Reader/Writer for topic **`A`** in a given partition **`P`**, then the equivalent zenoh resource will be named as **`/P/A`**. If no partition is defined, the equivalent zenoh resource will be named as **`/A`**.

Optionally, the bridge can be configured with a **scope** that will be used as a prefix to each zenoh resource. That is, for scope **`/S`** the equivalent zenoh resource will be **`/S/P/A`** for a topic **`A`** and a partition **`P`**, and **`/S/A`** for a topic without partition.

## How to build it
In order to build the zenoh bridge for DDS you need first to install the following dependencies:

- [Rust](https://www.rust-lang.org/tools/install)
- On Linux, make sure the `llvm` and `clang` development packages are installed:
   - on Debians do: `sudo apt install llvm-dev libclang-dev`
   - on CentOS or RHEL do: `sudo yum install llvm-devel clang-devel`
   - on Alpine do: `apk install llvm11-dev clang-dev`
- [CMake](https://cmake.org/download/) (to build CycloneDDS which is a native dependency)

Once these dependencies are in place, simply do:

```bash
$ git clone https://github.com/eclipse-zenoh/zenoh-plugin-dds.git
$ cd zenoh-plugin-dds
$ cargo build --release
```
The **zenoh-bridge-dds** binary will be generated in the `target/release` sub-directory.

## How to test it
Assuming you want to try this with ROS2, then install it by following the instructions available [here](https://index.ros.org/doc/ros2/Installation/Foxy/).
Once you've installed ROS2, you easily let ROS applications communicate across the internet.
That said the simplest way to quicky try out the zenoh bridge for DDS is to use it to bridge ROS2/DDS communnication across different ROS2/DDS domains.
This can be quickly tested on a single machine or on two (or more) machines connected to the same network. More complicated deployments with
multiple networks and zenoh routers across the Internet are just extensions of this basic scenario.

Let's assume that we have one ROS2 application running on domain **21** and another running on domain **42**. This alone will ensure
that the two applications won't be able to discover or echange data. As a test you can try to run them as shown below without
starting **zenoh-bridge-dds**  -- you remark  that nothing flows between the two. If you have two machines connected to the same network then run
these commands on one of them:


```
$ ROS_DOMAINID=21 ros2 run demo_nodes_py listener

$ ./target/release/zenoh-bridge-dds --scope /demo/dds -m peer -d 21
```

and these commands on the other:

```
$ ROS_DOMAIN_ID=42 ros2 run demo_nodes_cpp talker

$ ./target/release/zenoh-bridge-dds --scope /demo/dds -m peer -d 42
```

Otherwise, just run them on the same machine, you will see a stream of ROS2 *Hello World* messages coming across. Once again, as the ROS2 applications are using different domains, they are unable to discover and communicate, thus the data you see is flowing over zenoh.

### zenoh-bridge-dds command line arguments

`zenoh-dds-bridge` accepts the following arguments:
 * zenoh-related arguments:
   - `-m, --mode <MODE>` : The zenoh session mode. Default: `peer` Possible values: `peer` or `client`.  
      See [zenoh documentation](https://zenoh.io/docs/getting-started/key-concepts/#deployment-units) for more details.
   - `-l, --listener <LOCATOR>` : The locators the bridge will listen on for zenoh protocol. Can be specified multiple times. Example of locator: `tcp/localhost:7447`.
   - `-e, --peer <LOCATOR>` : zenoh peers locators the bridge will try to connect to (typically another bridge or a zenoh router). Example of locator: `tcp/<ip-address>:7447`.
   - `--no-multicast-scouting` : disable the zenoh scouting protocol that allows automatic discovery of zenoh peers and routers.
   - `--rest-plugin` : activate the [zenoh REST API](https://zenoh.io/docs/apis/apis/#rest-api), available by default on port 8000.
   - `--rest-http-port <rest-http-port>` : set the REST API http port (default: 8000)
 * DDS-related arguments:
   - `-d, --dds-domain <ID>` : The DDS Domain ID (if using with ROS this should be the same as `ROS_DOMAIN_ID`)
   - `-s, --dds-scope <String>` : A string used as prefix to scope DDS traffic when mapped to zenoh resources.
   - `-a, --dds-allow <String>`:  A regular expression matching the set of 'partition/topic-name' that should
     be bridged. By default, all partitions and topic are allowed.  
     Examples of expressions: 
        - `.*/TopicA` will allow only the `TopicA` to be routed, whatever the partition.
        - `PartitionX/.*` will allow all the topics to be routed, but only on `PartitionX`.
        - `cmd_vel|rosout` will allow only the topics containing `cmd_vel` or `rosout` in their name or partition name to be routed.
   - `-w, --dds-generalise-pub <String>` :  A list of key expressions to use for generalising the declaration of
     the zenoh publications, and thus minimizing the discovery traffic (usable multiple times).
     See [this blog](https://zenoh.io/blog/2021-03-23-discovery/#leveraging-resource-generalisation) for more details.
   - `-r, --dds-generalise-sub <String>` :  A list of key expressions to use for generalising the declaration of
     the zenoh subscriptions, and thus minimizing the discovery traffic (usable multiple times).
     See [this blog](https://zenoh.io/blog/2021-03-23-discovery/#leveraging-resource-generalisation) for more details.

### Admin space

The zenoh bridge for DDS exposes and administration space allowing to browse the DDS entities that have been discovered (with their QoS), and the routes that have been established between DDS and zenoh.
This administration space is accessible via any zenoh API, including the REST API.

The `zenoh-dds-bridge` exposes this administration space with paths prefixed by `/@/service/<uuid>/dds` (where `<uuid>` is the unique identifier of the bridge instance). The informations are then organized with such paths:
 - `/@/service/<uuid>/dds/config` : the bridge configuration
 - `/@/service/<uuid>/dds/participant/<gid>/reader/<gid>/<topic>` : a discovered DDS reader on `<topic>`
 - `/@/service/<uuid>/dds/participant/<gid>/writer/<gid>/<topic>` : a discovered DDS reader on `<topic>`
 - `/@/service/<uuid>/dds/route/from_dds/<zenoh-resource>` : a route established from a DDS writer to a zenoh resource named `<zenoh-resource>` (see [mapping rules](#mapping-dds-topics-to-zenoh-resources)).
 - `/@/service/<uuid>/dds/route/to_dds/<zenoh-resource>` : a route established from a zenoh resource named `<zenoh-resource>` (see [mapping rules](#mapping-dds-topics-to-zenoh-resources))..

Example of queries on administration space using the REST API with the `curl` command line tool
(don't forget to activate the REST API with `--rest-plugin` argument):
 - List all the DDS entities that have been discovered:
    ```bash
    curl http://localhost:8000:/@/service/**/participant/**
    ```
 - List all established routes:
    ```bash
    curl http://localhost:8000:/@/service/**/route/**
    ```
 - List all discovered DDS entities and established route for topic `cmd_vel`:
    ```bash
    curl http://localhost:8000:/@/service/**/cmd_vel
    ```

> _Pro tip: pipe the result into [**jq**](https://stedolan.github.io/jq/) command for JSON pretty print or transformation._

### Troubleshooting
In case you do not see any data flowing around when running  on different computers across a network, it may be due to your network does not allowing for multicast - this latter is used for scouting in zenoh. The simplest way to fix this issue is to explicitely pass locators as described next.

On one of the two computers which we'll call computer-a run:

```
$ ROS_DOMAIN_ID=21 ros2 run demo_nodes_py listener

$ ./target/release/zenoh-bridge-dds --scope /demo/dds -m peer -d 21 -l tcp/<computer-a-ip-address>:7447
```

and these commands on the other:

```
$ ROS_DOMAIN_ID=42 ros2 run demo_nodes_cpp talker

$ ./target/release/zenoh-bridge-dds --scope /demo/dds -m peer -d 42 -e tcp/<computer-a-ip-address>:7447
```

Where the <computer-a-ip-address> should be replaced by the IP address used to communicate on the
network to which both computers are connected. If you enconter any issues do not hesitate
to reach us out on [gitter](http://gitter.im/atolab/zenoh)

Good Hacking!
