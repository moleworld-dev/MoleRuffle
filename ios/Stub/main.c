// 占位 main:只为让 Xcode 产出一个可执行壳,随后被 postCompile 脚本用真正的
// Rust 二进制覆盖。最终 .app 的可执行体是 Rust 编出来的 moleruffle。
int main(void) { return 0; }
