"""Independent stdlib-only wire vectors. Fixed PUBLIC TEST key only; no wallet/network.

BIP340 signing below is a fixture generator, never production cryptography.
Production verification uses the existing secp256k1 library.
"""
import base64, hashlib, json
from pathlib import Path
body = {'schema':'maxplayer.content.v2','job_id':'10'*32,'offer_id':None,'award_id':None,'message_id':'20'*32,'type':'task','revision':0,'supersedes':None,'author':'11'*32,'recipients':['11'*32,'22'*32,'33'*32],'text':'vector task ☃','requested_output':'text/plain','dispatch':{},'attachments':[]}
compact = lambda v: json.dumps(v,ensure_ascii=False,separators=(',',':'))
body_bytes = compact(body).encode()
nonce=bytes.fromhex('44'*32)
commit=hashlib.sha256(b'maxplayer/content/v2\0'+nonce+len(body_bytes).to_bytes(8,'big')+body_bytes).hexdigest()
envelope=compact({'schema':'maxplayer.content-envelope.v2','nonce':nonce.hex(),'body_b64':base64.b64encode(body_bytes).decode()})
offer='55'*32
job_hash=hashlib.sha256(b'maxplayer/job/v2\0'+bytes.fromhex(offer)).hexdigest()
# Secp256k1 group arithmetic, deliberately independent of the Rust implementation.
P=2**256-2**32-977
N=0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
G=(0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798,
   0x483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8)
def add(a,b):
    if a is None:return b
    if b is None:return a
    x,y=a;u,v=b
    if x==u and (y+v)%P==0:return None
    slope=((3*x*x)*pow(2*y,-1,P) if a==b else (v-y)*pow(u-x,-1,P))%P
    q=(slope*slope-x-u)%P
    return q,(slope*(x-q)-y)%P
def mul(k):
    result=None;point=G
    while k:
        if k&1:result=add(result,point)
        point=add(point,point);k>>=1
    return result
def tagged(tag,data):
    h=hashlib.sha256(tag.encode()).digest()
    return hashlib.sha256(h+h+data).digest()
def sign(message):
    secret=3 # Published fixture key. Never used outside this generator.
    x,y=mul(secret)
    d=secret if y%2==0 else N-secret
    pub=x.to_bytes(32,'big')
    mask=tagged('BIP0340/aux',bytes(32))
    t=bytes(a^b for a,b in zip(d.to_bytes(32,'big'),mask))
    k0=int.from_bytes(tagged('BIP0340/nonce',t+pub+message),'big')%N
    rx,ry=mul(k0);k=k0 if ry%2==0 else N-k0
    r=rx.to_bytes(32,'big')
    e=int.from_bytes(tagged('BIP0340/challenge',r+pub+message),'big')%N
    return pub.hex(),(r+((k+e*d)%N).to_bytes(32,'big')).hex()
# Published BIP340 test vector 0 pins this generator itself.
assert sign(bytes(32))[1] == ('e907831f80848d1069a5371b402410364bdf1c5f8307b0084c55f1ce2dca8215'
                           '25f66a4a85ea8b71e482a74f382d2ce5ebeee8fdb2172f477df4900d310536c0')
receipts=[]
for kind,integrity in [('fork','66'*20),('inline',commit)]:
    for paid in [False,True]:
        values=['maxplayer/v2/receipt-preimage',job_hash,offer,100,'sat','11'*32,'22'*32,integrity,kind,'none']
        if paid:values.append('77'*32)
        preimage=compact(values)
        pub,sig=sign(hashlib.sha256(preimage.encode()).digest())
        receipts.append({'signer':pub,'signature':sig,'kind':kind,'paid':paid,'integrity':integrity,'preimage':preimage,'digest':hashlib.sha256(preimage.encode()).hexdigest()})
Path(__file__).with_name('vectors.json').write_text(json.dumps({'body':body_bytes.decode(),'envelope':envelope,'commitment':commit,'offer_id':offer,'job_hash':job_hash,'receipts':receipts},ensure_ascii=False,indent=2)+'\n')
