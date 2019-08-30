### native p2p

impl:

1. 客户端通过HTTP协议随机给节点发包
2. 节点对消息按ID进行排序，并计算Hash，节点hash结果应跟客户端一致，多个节点的Hash应该一致
3. 节点支持随时重启功能，可以kill任一节点并重启，hash结果应该保持和测试端一致
4. 节点支持基本的P2P功能，包括节点发现，消息路由
5. 可以使用rust blocking io/mio/tokio 包括不稳定特性如async/await等实现
6. 客户端代码在 https://github.com/fanngyuan/performance-client ，客户端发的每条消息都做了Base64，URL encoding，使用sha256.Sum256计算hash
