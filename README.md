### native p2p

#### solve

1. 客户端通过HTTP协议随机给节点发包
2. 节点对消息按ID进行排序，并计算Hash，节点hash结果应跟客户端一致，多个节点的Hash应该一致
3. 节点支持随时重启功能，可以kill任一节点并重启，hash结果应该保持和测试端一致
4. 节点支持基本的P2P功能，包括节点发现，消息路由
5. 可以使用rust blocking io/mio/tokio 包括不稳定特性如async/await等实现
6. 客户端代码在 https://github.com/fanngyuan/performance-client ，客户端发的每条消息都做了Base64，URL encoding，使用sha256.Sum256计算hash


### Give a try

开多个 shell，依次执行：

```shell script
./target/debug/naive_p2p --config configs/tom.toml 
```

```shell script
./target/debug/naive_p2p --config configs/alice.toml 
```

```shell script
./target/debug/naive_p2p --config configs/bob.toml 
```

用 `https://github.com/fanngyuan/performance-client` 写入数据：

```shell script
./client -times 1000 -url http://localhost:8080,http://localhost:8081,http://localhost:8082
```

client 给出 hash：`ee3d211f26b3b3d3468d269b9cc57209ff3276952622bca5bd5aa2aaa5fdc27a`

查看 p2p node 的hash：

- tom: `http :8080/state`
- alice: `http :8081/state`
- bob: `http :8082/state`



### How it work

#### peer discovery

1. new peer 广播
2. node 定时的 去  random outbound peer 拿它的 random inbound peer。 

#### block sync

1. new block broadcast
2. 节点之间通过 hearbeat 同步 lastest block index，如果发现自己落后，就主动去同步数据。

#### storage

内存实现。节点重启，数据丢失，重新从 bootnodes 去 sync。