## Writing

rename: Log replay involving data, names, xattrs, new inodes or other subvolumes
| Log replay of extended references that the log logs or removes for an inode it logs only to say it exists, hashed directory ranges (`DIR_LOG_ITEM`, which a 6.1 kernel never writes, #408), a write inside part of a compressed extent, and a removed name whose inode is a directory or keeps other names | Refused for writing | | #394: #409, #410, #411, #475 | `log_tree_kernel.rs` |
