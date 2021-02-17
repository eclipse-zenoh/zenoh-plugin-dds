#!/bin/bash

pushd $PWD &>/dev/null

if [ $target == "Linux" -a `id -u` != 0 ];
then
    sudo rm -Rf deps &> /dev/null
else
    rm -Rf deps &> /dev/null
fi

Unameout="$(uname -s)"
case "${unameOut}" in
    Linux*)     target=Linux;;
    *)          target=Other
esac

if [[ -v CYCLONE_INCLUDE ]] && [[ -v CYCLONE_LIB ]]; then
    echo "Looking for Cyclone installation at: $CYCLONE_ROOT"
    if [ ! -e $CYCLONE_INCLUDE/include/dds/ddsc/dds_public_impl.h  ]; then
	echo "Could not find cyclone installation at $CYCLONE_ROOT"
	echo "please verify your cyclone installation."
	echo "\t[Hint: does the file $CYCLONE_ROOT/include/dds/ddsc/dds_public_impl.h exist?"
	echo "[-.-]"
	exit
    fi
else
    echo "The environment variables CYCLONE_INCLUDE and CYCLONE_LIB are not set,"
    echo "checking for existing installations..."
    echo "[-.-]"

    if [ ! -e /usr/local/include/dds/ddsc/dds_public_impl.h  ] && [ ! -e /opt/ros/$ROS_DISTRO/include/dds/ddsc/dds_public_impl.h ];
    then
	echo "Cound not find any installation, buiding from sources..."
	mkdir deps &>/dev/null
	cd deps

	git clone --depth 1 -b 0.7.0 https://github.com/eclipse-cyclonedds/cyclonedds.git
	mkdir cyclonedds/build
	cd cyclonedds/build
	cmake ..
	if [ $target == "Linux" -a `id -u` != 0 ];	
	then
            sudo make install
	else
            make install
	fi
    else
	if [ -e /usr/local/include/dds/ddsc/dds_public_impl.h  ];
	then
	    CYCLONE_INCLUDE=/usr/local/include
	    CYCLONE_LIB=/usr/local/lib
	elif [ -e /opt/ros/$ROS_DISTRO/include/dds/ddsc/dds_public_impl.h ];
	then
	    CYCLONE_INCLUDE=/opt/ros/$ROS_DISTRO/include
	    CYCLONE_LIB=/opt/ros/$ROS_DISTRO/lib/`arch`-$OSTYPE
	fi
	echo "Found Cyclone installed at $CYCLONE_INCLUDE and $CYCLONE_LIB, skipping installation..."
    fi
fi

popd &>/dev/null
pushd $PWD &>/dev/null
echo "[-.-]"
if [ !  -e /usr/local/include/cdds/cdds_util.h  ];
then
    echo "Installing Cyclocut utilities libraries..."
    mkdir deps &>/dev/null
    cd deps

    git clone https://github.com/kydos/cyclocut.git
    mkdir cyclocut/build
    cd cyclocut/build
    cmake -DCYCLONE_INCLUDE=$CYCLONE_INCLUDE -DCYCLONE_LIB=$CYCLONE_LIB ..
    if [ $target == "Linux" -a `id -u` != 0 ];
    then
        sudo make install
    else
        make install
    fi
    cd ../../..
else
    echo "Cyclocut is already installed, skipping installation."
    echo "[-.-]"
fi
popd &>/dev/null
hash cargo 2>/dev/null
if [[ ( "$?" != 0 ) && $target == "Linux" ]];
then
    curl https://sh.rustup.rs -sSf | sh
    rustup default nightly
else
    echo "Cargo is already installed, setting up nightly."
    rustup default nightly
fi
echo "Cleaning up."

if [ $target == "Linux" -a `id -u` != 0 ];
then
    sudo rm -Rf deps
else
    rm -Rf deps
fi

echo "Done [^_^]"
echo ""
echo "Please set the following environment variables"
echo "   export CYCLONE_INCLUDE=$CYCLONE_INCLUDE"
echo "   export CYCLONE_LIB=$CYCLONE_LIB"
echo ""
echo "Then run:"
echo '    "cargo build --release"'
echo ""
echo "If you have any questions reach us out on https://gitter.im/atolab/zenoh"
