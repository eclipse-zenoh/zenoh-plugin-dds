![zenoh banner](http://zenoh.io/img/zenoh-dragon-small.png)

[![CI](https://github.com/eclipse-zenoh/zenoh-plugin-dds/workflows/Rust/badge.svg)](https://github.com/eclipse-zenoh/zenoh-plugin-dds/actions?query=workflow%3ARust)
[![License](https://img.shields.io/badge/License-EPL%202.0-blue)](https://choosealicense.com/licenses/epl-2.0/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

# Zenoh plugin for DDS

## Background
The Data Distribution Service (DDS) is a standard for data-centric publish subscribe. Whilst DDS has been around for quite some time and has a long history of deployments in various industries, it has recently gained quite a bit of attentions thanks to its adoption by the Robotic Operating System (ROS2) -- where it is used for communication between ROS2 nodes.

## Robot Swarms and Edge Robotics
As mentioned above, ROS2 has adopted DDS as the mechanism to exchange data between nodes within and potentially across a robot. That said, due to some of the very core assumptions at the foundations of the DDS wire-protocol, beside the fact that it  leverage UDP/IP multicast for communication,  it is not so straightforward to scale DDS communication over a WAN or across multiple LANs.  Zenoh, on the other hand was designed since its inception to operate at Internet Scale.

![zenoh-plugin-dds](http://zenoh.io/img/wiki/zenoh-plugin-dds.png)

Thus, the main motivations to have a **DDS connector** for **zenoh** are:

- Facilitate the interconnection of robot swarms.
- Support use cases of edge robotics.
- Give the possibility to use **zenoh**'s geo-distributed storage and query system to better manage robot's data.

## Architecture
**zenoh** routers provide a plug-in mechanism that allow for extensions to be loaded and activated by its management API. Thus the most natural way to implement a DDS connector for zenoh is to do that as a zenoh router plugin.

This plugin, will essentially:

- Spoof DDS discovery data and transparently expose DDS writers/readers as zenoh publisher/subscribers
- Route the data produced by discovered DDS writers to data to matching entities.

Beside the zenoh router plugin we also support a stand-alone bridge called **zenoh-bridge-dds** that can be used to transparently bridge DDS data on zenoh and viceversa.

### Mapping DDS to zenoh
The mapping between DDS and zenoh is rather straightforward. Given a DDS Reader/Writer for topic ```A``` in a given partition ```P``` with a set of QoS ```Q```, then the equivalent zenoh resource will be named as ```P/A/*```. On the other hand actual writes will be on the resource ```/P/A/sample-key-hash``` as this allows for zenoh subscriber to easily subscribe to just a specific Topic instance, a set of them or of all of them.


## Trying it Out
In order to get running with the DDS plugin for zenoh you need first to install the following dependencies:

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
