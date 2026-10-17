## Writing

rename: Log replay involving data, names, xattrs, new inodes or other subvolumes
| Log replay of device, FIFO and socket nodes, hard links, extended references, hashed directory ranges, a write inside part of a compressed extent, and a removed name whose inode is a directory or keeps other names | Refused for writing | | #394: #405, #406, #407, #408, #409, #410, #411 | `log_tree_kernel.rs` |
