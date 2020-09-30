![zenoh banner](./zenoh-dragon.png)

![Rust](https://github.com/eclipse-zenoh/zenoh-plugin-dds/workflows/Rust/badge.svg)
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
 
Beside the zenoh router plugin we also support a stand-alone bridge called **dzd** that can be used to transparently bridge DDS data on zenoh and viceversa.

### Mapping DDS to zenoh
The mapping between DDS and zenoh is rather straightforward. Given a DDS Reader/Writer for topic ```A``` in a given partition ```P``` with a set of QoS ```Q```, then the equivalent zenoh resource will be named as ```P/A/*```. On the other hand actual writes will be on the resource ```P/A/sample-key-hash``` as this allows for zenoh subscriber to easily subscribe to just a specific Topic instance, a set of them or of all of them.


## Trying it Out
In order to get running with the DDS plugin for zenoh you need first to install the following dependencies:

- [CMake](https://cmake.org/download/)
- Your favourite C/C++ Compiler
- [Rust](curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh)

Once these dependencies are in place, simply do:

```
$ git clone https://github.com/eclipse-zenoh/zenoh-plugin-dds.git
$ cd zenoh-plugin-dds
$ . ./configure.sh
$ cargo build --all-targets
```
