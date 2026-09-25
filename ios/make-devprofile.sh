#!/bin/bash
# 生成/更新【含本机设备】的 iOS 开发描述文件 "MoleRuffle Dev CLI",装到本机供 build-device.sh 手动签名用。
#
# 为什么需要它:xcodebuild 自动签名(-allowProvisioningUpdates)在设备未连接时,
# 会生成一个【0 设备】的开发描述文件。装到真机后 iOS 查不到本机 UDID → 启动即弹回桌面
# (现象=装得上、一点就闪退,无崩溃日志)。这个坑排查了很久,根因就是空设备列表。
# 解法:用 ASC API 明确把设备 UDID 塞进开发描述文件,绕开自动签名的抽风。
#
# 何时重跑:换调试机、描述文件过期(1年)、或换开发证书时。
# 用法:ios/make-devprofile.sh [设备UDID]   # 不传则用默认(17PM)
set -e

UDID="${1:-00008150-001E10A62132401C}"     # 默认 xcm's iPhone 17 Pro Max
BUNDLE="com.moleworld.moleruffle"
PROFILE_NAME="MoleRuffle Dev CLI"
# 本机有私钥的 Apple Development 证书 SHA1。自动读取,不写死 —— 证书续签/重发后旧值会让
# 描述文件与签名证书对不上(xcodebuild 报 "doesn't include signing certificate")。可用环境变量覆盖。
LOCAL_CERT_SHA1="${LOCAL_CERT_SHA1:-$(security find-identity -v -p codesigning | awk '/Apple Development/{print $2; exit}')}"
[ -n "$LOCAL_CERT_SHA1" ] || { echo "✗ 本机钥匙串里没有 Apple Development 证书" >&2; exit 1; }
echo "使用本机开发证书 SHA1: $LOCAL_CERT_SHA1"

KEY="$HOME/.appstoreconnect/private_keys/AuthKey_6N5DAM7RXC.p8"
KEYID="6N5DAM7RXC"
ISSUER="0f1cb134-9497-45fb-959c-09fb3a7cf633"

"${PYTHON:-$(command -v python3)}" - "$UDID" "$BUNDLE" "$PROFILE_NAME" "$LOCAL_CERT_SHA1" "$KEY" "$KEYID" "$ISSUER" <<'PYEOF'
import sys, jwt, time, json, urllib.request, urllib.parse, base64, hashlib, os
UDID,BUNDLE,PROFILE_NAME,CERT_SHA1,KEY_PATH,KEY_ID,ISSUER = sys.argv[1:8]
tok=jwt.encode({"iss":ISSUER,"iat":int(time.time()),"exp":int(time.time())+900,"aud":"appstoreconnect-v1"},
    open(KEY_PATH).read(),algorithm="ES256",headers={"kid":KEY_ID})
def req(method,p,payload=None):
    r=urllib.request.Request("https://api.appstoreconnect.apple.com"+p,method=method,
        headers={"Authorization":f"Bearer {tok}","Content-Type":"application/json"},
        data=json.dumps(payload).encode() if payload else None)
    try:
        resp=urllib.request.urlopen(r); return resp.status,(json.load(resp) if resp.status!=204 else {})
    except urllib.error.HTTPError as e: return e.code,e.read().decode()[:400]

# 1. 证书:按 SHA1 找本机私钥对应那张
_,c=req("GET","/v1/certificates?filter[certificateType]=DEVELOPMENT&limit=20")
cert_id=None
for x in c["data"]:
    if hashlib.sha1(base64.b64decode(x["attributes"]["certificateContent"])).hexdigest().upper()==CERT_SHA1.upper():
        cert_id=x["id"]
assert cert_id, f"没找到 SHA1={CERT_SHA1} 的开发证书(本机私钥那张)"

# 2. bundleId 资源
_,b=req("GET","/v1/bundleIds?filter[identifier]="+urllib.parse.quote(BUNDLE)+"&limit=5")
bundle_id=b["data"][0]["id"]

# 3. 设备(未注册则注册)
_,d=req("GET","/v1/devices?limit=200")
dev_id=None
for x in d["data"]:
    if x["attributes"].get("udid","").replace("-","").upper()==UDID.replace("-","").upper(): dev_id=x["id"]
if not dev_id:
    s,r=req("POST","/v1/devices",{"data":{"type":"devices","attributes":{"name":"MoleRuffle Dev Device","platform":"IOS","udid":UDID}}})
    assert s<400, f"注册设备失败: {r}"
    dev_id=r["data"]["id"]; print("已注册新设备:", UDID)

# 4. 删同名旧 profile
_,p=req("GET","/v1/profiles?filter[name]="+urllib.parse.quote(PROFILE_NAME)+"&limit=5")
if isinstance(p,dict):
    for x in p.get("data",[]): req("DELETE",f"/v1/profiles/{x['id']}")

# 5. 建新 profile
s,r=req("POST","/v1/profiles",{"data":{"type":"profiles",
    "attributes":{"name":PROFILE_NAME,"profileType":"IOS_APP_DEVELOPMENT"},
    "relationships":{"bundleId":{"data":{"type":"bundleIds","id":bundle_id}},
        "certificates":{"data":[{"type":"certificates","id":cert_id}]},
        "devices":{"data":[{"type":"devices","id":dev_id}]}}}})
assert s<400, f"建 profile 失败: {r}"
a=r["data"]["attributes"]; uuid=a["uuid"]
outdir=os.path.expanduser("~/Library/MobileDevice/Provisioning Profiles"); os.makedirs(outdir,exist_ok=True)
path=os.path.join(outdir,uuid+".mobileprovision")
open(path,"wb").write(base64.b64decode(a["profileContent"]))
print(f"✅ 已装开发描述文件 '{PROFILE_NAME}'")
print(f"   设备 {UDID} | 到期 {a['expirationDate']} | UUID {uuid}")
PYEOF
echo "现在可跑: ios/build-device.sh $UDID"
