import json, os, sqlite3, subprocess, sys, tempfile, time, statistics

root=tempfile.TemporaryDirectory(); path=os.path.join(root.name,'library.db')
c=sqlite3.connect(path); c.executescript('CREATE TABLE schema_meta(key TEXT PRIMARY KEY,value TEXT); INSERT INTO schema_meta VALUES ("db_version","32"),("db_schema_version","6"); CREATE TABLE book(id INTEGER PRIMARY KEY,title TEXT,totalLines INTEGER); INSERT INTO book VALUES(1,"book",10000); CREATE TABLE line(id INTEGER PRIMARY KEY,bookId INTEGER,lineIndex INTEGER,heRef TEXT,content TEXT); CREATE INDEX line_book_index ON line(bookId,lineIndex);')
c.executemany('INSERT INTO line VALUES(?,1,?,?,?)',[(i+1,i,'ref', '<b>טקסט לדוגמה</b> '*8) for i in range(10000)]); c.commit();c.close()
def run(binary, limit, iterations=600):
 p=subprocess.Popen([binary,'--db',path],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True,bufsize=1)
 def send(v): p.stdin.write(json.dumps(v,ensure_ascii=False)+'\n');p.stdin.flush()
 def req(i): return {'jsonrpc':'2.0','id':i,'method':'tools/call','params':{'name':'get_text','arguments':{'book_id':1,'line_index':(i*11)%9900,'limit':limit}}}
 send({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-03-26','capabilities':{},'clientInfo':{'name':'bench','version':'1'}}}); assert 'result' in json.loads(p.stdout.readline())
 send({'jsonrpc':'2.0','method':'notifications/initialized'})
 vals=[]
 for i in range(iterations+50):
  t=time.perf_counter_ns();send(req(i+2)); msg=json.loads(p.stdout.readline());dt=(time.perf_counter_ns()-t)/1e6
  assert 'result' in msg, msg
  if i>=50: vals.append(dt)
 p.stdin.close(); p.wait(timeout=5)
 return {'median_ms':statistics.median(vals),'p95_ms':sorted(vals)[int(len(vals)*.95)-1]}
for limit in (1,50):
 for binary in sys.argv[1:]:
  samples=[run(binary,limit) for _ in range(3)]
  print(json.dumps({'binary':binary,'limit':limit,'samples':samples,'median_of_medians_ms':statistics.median(x['median_ms'] for x in samples),'median_p95_ms':statistics.median(x['p95_ms'] for x in samples)}),flush=True)
