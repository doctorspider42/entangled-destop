#!/bin/sh

REPO="https://github.com/KhronosGroup/Vulkan-Headers"

if [ "$#" -ne 1 ]; then
    echo "Usage: $0 <Vulkan-Headers-Git-Tag>"
    exit 1
fi

BASEDIR="$(dirname "$(dirname "$0")")"
if [ -e "$BASEDIR/.git" -a \
     -d "$BASEDIR/include" -a \
     -f "$BASEDIR/xmls/vk.xml" ]; then
    echo "Using $BASEDIR as the base directory"
else
    echo "Cannot update $BASEDIR"
    exit 1
fi

TMPDIR=$(mktemp -d)
echo "Using $TMPDIR as the temporary directory"

URL="$REPO/archive/refs/tags/$1.tar.gz"
echo "Downloading $URL..."
wget -q -O - "$URL" | tar -C "$TMPDIR" -zx

DOWNLOADED="$TMPDIR/Vulkan-Headers-${1#v}"

if [ -d "$DOWNLOADED/include" ]; then
    echo "Updating $BASEDIR/include/"
    rm -r "$BASEDIR/include"
    cp -r "$DOWNLOADED/include" "$BASEDIR/include"

    # Remove the c++ headers. They are 10MB and we never use them.
    rm -f "$BASEDIR"/include/vulkan/*.hpp

    # Remove the .cppm files
    rm -f "$BASEDIR"/include/vulkan/*.cppm
fi

if [ -f "$DOWNLOADED/registry/vk.xml" ]; then
    echo "Updating $BASEDIR/xmls/vk.xml"
    cp "$DOWNLOADED/registry/vk.xml" "$BASEDIR/xmls/vk.xml"
fi

echo "Removing $TMPDIR"
rm -r "$TMPDIR"
