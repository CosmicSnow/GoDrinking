import WebSocket from 'ws'
const base = "https://together.jouymaker.com"
const code = "WSGODR"
const nick = "HostWS2"

async function post(path, body){
  const r = await fetch(base + path, {method:"POST", headers:{"Content-Type":"application/json"}, body: JSON.stringify(body)})
  const j = await r.json()
  console.log(path, r.status, JSON.stringify(j).slice(0,400))
  return {status:r.status, json:j}
}
console.log("open")
let open = await post("/v1/host/open", {code, nickname:nick, admission:true})
if(!open.json.ok){ console.log("open fail"); process.exit(1)}
let token = open.json.host_token
console.log("token", token.slice(0,8))
let wsUrl = base.replace(/^http/,"ws") + `/v1/ws?role=host&token=${token}`
console.log("connect", wsUrl)
let ws = new WebSocket(wsUrl)
await new Promise((res,rej)=>{
  let t=setTimeout(()=>rej(new Error("timeout host ws")),5000)
  ws.on("open", ()=>{clearTimeout(t); console.log("host WS open"); res()})
  ws.on("error", e=>{clearTimeout(t); console.log("host WS error",e.message); rej(e)})
  ws.on("unexpected-response", (req,res)=>{ console.log("unexpected",res.statusCode, res.statusMessage); res.on("data",d=>console.log(d.toString())) })
})
ws.on("message", d=> console.log("host msg", d.toString().slice(0,600)))
console.log("viewer ask")
let ask = await post("/v1/viewer/ask", {code, nickname:"ViewerWS2"})
console.log("ask", ask.json)
let vtoken = ask.json.viewer_token
let ws2Url = base.replace(/^http/,"ws") + `/v1/ws?role=viewer&token=${vtoken}`
console.log("viewer connect", ws2Url)
let ws2 = new WebSocket(ws2Url)
await new Promise((res,rej)=>{
  let t=setTimeout(()=>rej(new Error("timeout viewer ws")),5000)
  ws2.on("open", ()=>{clearTimeout(t); console.log("viewer WS open"); res()})
  ws2.on("error", e=>{clearTimeout(t); console.log("viewer WS error",e.message); rej(e)})
})
ws2.on("message", d=> console.log("viewer msg", d.toString().slice(0,600)))
console.log("heartbeat")
let hb = await post("/v1/host/heartbeat", {host_token: token})
console.log("hb", hb.json)
await new Promise(r=>setTimeout(r,1500))
ws.close(); ws2.close()
console.log("done")
