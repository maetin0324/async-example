# Wake機構を作る
ここまでで単純なFutureのみを動かす実装が完成した。ここからはWakerを扱い、中断されたFutureを再開する仕組みを作っていく。

## RawWaker
