// Control for "N workers = N concurrent provider calls": hold the number of
// director threads at 8 and vary only how many worker loops serve them.
'use strict';
// Run bench.cjs first: it is what stages spike.node from the release build.
const path=require('path');
const fs=require('fs');
const {Worker}=require('worker_threads');
const ADDON=path.join(__dirname,'spike.node');
if(!fs.existsSync(ADDON))throw new Error('spike.node missing — run `node bench.cjs` first, it stages it');
const native=require(ADDON);
function spawn(){return new Promise((res,rej)=>{const w=new Worker(path.join(__dirname,'provider-worker.cjs'),{workerData:{addon:ADDON,mode:'sync'}});w.once('error',rej);w.once('message',m=>m.ready&&res({worker:w,info:m}));});}
(async()=>{
  const ws=[];for(let i=0;i<8;i++)ws.push(await spawn());
  const ids=ws.map(w=>w.info.bridgeId);
  console.log('threads=8, varying worker loops (64 MiB per thread, 4 KiB reads, raw)');
  console.log('loops  MiB/s     p50 us   p99 us');
  for(const n of [1,2,4,8]){
    const r=await native.benchRead({bridges:ids.slice(0,n),threads:8,fileSize:64*1024*1024,readSize:4096,cached:false,blockSize:1<<20,label:`loops=${n}`});
    console.log(String(n).padStart(5),r.mibPerSec.toFixed(1).padStart(9),r.p50Us.toFixed(2).padStart(9),r.p99Us.toFixed(2).padStart(9),'bad='+r.badPayloadReads);
  }
  for(const w of ws)w.worker.postMessage({cmd:'exit'});
  setTimeout(()=>process.exit(0),200);
})().catch(e=>{console.error(e);process.exit(1)});
