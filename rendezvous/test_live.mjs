import WebSocket from 'ws'
const base = "https://together.jouymaker.com"
const code = "TST" + Math.random().toString(36).slice(2,5).toUpperCase()
console.log("code",code)
async function post(p,b){ const r=await fetch(base+p,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(b)}); const j=await r.json(); console.log(p, r.status, JSON.stringify(j).slice(0,300)); return j}
let o=await post("/v1/host/open",{code, nickname:"HostTest", admission:true})
if(!o.ok){ console.log("open fail"); process.exit(1)}
let token=o.host_token
let wsUrl=base.replace("https","wss")+"/v1/ws?role=host&token="+token
console.log("ws host",wsUrl)
let ws=new WebSocket(wsUrl)
await new Promise((res,rej)=>{ let t=setTimeout(()=>rej(new Error("timeout host")),4000); ws.on("open",()=>{clearTimeout(t); console.log("host WS open"); res()}); ws.on("error",e=>{clearTimeout(t); console.log("host err",e.message); rej(e)}); ws.on("unexpected-response",(q,r)=>{console.log("unexpected",r.statusCode)})})
ws.on("message",d=>console.log("host msg",d.toString().slice(0,500)))
let ask=await post("/v1/viewer/ask",{code, nickname:"ViewerTest"})
console.log("ask",ask)
let vtoken=ask.viewer_token
let ws2=new WebSocket(base.replace("https","wss")+"/v1/ws?role=viewer&token="+vtoken)
await new Promise((res,rej)=>{ let t=setTimeout(()=>rej(new Error("timeout viewer")),4000); ws2.on("open",()=>{clearTimeout(t); console.log("viewer WS open"); res()}); ws2.on("error",e=>{clearTimeout(t); console.log("viewer err",e.message); rej(e)})})
ws2.on("message",d=>console.log("viewer msg",d.toString().slice(0,500)))
console.log("heartbeat")
let hb=await post("/v1/host/heartbeat",{host_token:token})
console.log("hb",hb)
setTimeout(()=>{ ws.close(); ws2.close(); console.log("done ok"); },800)
