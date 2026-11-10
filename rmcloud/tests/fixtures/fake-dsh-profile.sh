#!/bin/sh
# 测试用假 dsh：给错题档案（POST /api/v1/profile/scan）用，不联网、不跑模型。
# 场景由 prompt 里的标记决定——测试把标记写在 scan 的 text 提示里，透传到这里。
# 注意 case 分支的顺序：先写更具体的标记。
usage='{"type":"status","phase":"step_end","step":1,"usage":{"inputTokens":120,"outputTokens":30}}'
done='{"type":"status","phase":"turn_end","reason":{"kind":"completed"}}'

case "$*" in
  *"场景:失败"*)
    # 模型侧失败：只发 error 事件、非 0 退出（不写库）。
    printf '%s\n' \
      '{"type":"status","phase":"turn_end","reason":{"kind":"error","message":"模型超时了"}}'
    exit 1
    ;;
  *"场景:空"*)
    printf '%s\n' \
      '{"type":"text","text":"{\"problems\":[]}"}' \
      "$usage" "$done"
    ;;
  *"场景:坏"*)
    # 括号配平但 JSON 非法。
    printf '%s\n' \
      '{"type":"text","text":"{\"problems\":[}"}' \
      "$usage" "$done"
    ;;
  *"场景:无"*)
    # 压根没有 JSON（只有废话）。
    printf '%s\n' \
      '{"type":"text","text":"这一页看不清，没法登记。"}' \
      "$usage" "$done"
    ;;
  *"场景:废话"*)
    # 前后废话 + 围栏 + 只有部分字段（没有 subject/difficulty/priority/repeat_of）。
    printf '%s\n' \
      '{"type":"text","text":"好的，我先看看这页：\n```json\n{\"problems\":[{\"text\":\"求 3x+1 的定义域\"}]}\n```\n以上就是全部，希望有帮助。"}' \
      "$usage" "$done"
    ;;
  *"场景:指认"*)
    # 新题，但模型说它和已有条目 1 是同一道（repeat_of 指认）。
    printf '%s\n' \
      '{"type":"text","text":"{\"problems\":[{\"text\":\"新题：求 ln x 的导数\",\"subject\":\"数学\",\"difficulty\":2,\"repeat_of\":\"1\",\"priority\":\"normal\"}]}"}' \
      "$usage" "$done"
    ;;
  *"场景:高甲"*)
    printf '%s\n' \
      '{"type":"text","text":"{\"problems\":[{\"text\":\"求 sin x 的最大值\",\"subject\":\"数学\",\"difficulty\":4,\"repeat_of\":\"\",\"priority\":\"high\"}]}"}' \
      "$usage" "$done"
    ;;
  *"场景:高乙"*)
    printf '%s\n' \
      '{"type":"text","text":"{\"problems\":[{\"text\":\"证明三角形内角和是 180 度\",\"subject\":\"数学\",\"difficulty\":5,\"repeat_of\":\"\",\"priority\":\"high\"}]}"}' \
      "$usage" "$done"
    ;;
  *"场景:低"*)
    printf '%s\n' \
      '{"type":"text","text":"{\"problems\":[{\"text\":\"求 1/2 + 1/3 的值\",\"subject\":\"数学\",\"difficulty\":1,\"repeat_of\":\"\",\"priority\":\"low\"}]}"}' \
      "$usage" "$done"
    ;;
  *)
    # 默认：一道数学题，优先级 low（再错一次应该升到 normal，第三次 high）。
    printf '%s\n' \
      '{"type":"text","text":"{\"problems\":[{\"text\":\"求 f(x)=x^2 在 x=2 处的导数\",\"subject\":\"数学\",\"difficulty\":3,\"repeat_of\":\"\",\"priority\":\"low\"}]}"}' \
      "$usage" "$done"
    ;;
esac
