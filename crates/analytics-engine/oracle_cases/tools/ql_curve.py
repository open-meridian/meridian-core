"""The Treasury curve oracle (contract v29, W12.3): treasury-curve.json.

Test-only tooling, never shipped: QuantLib 1.40 (the SDK's pinned release)
fits PiecewiseConvexMonotoneForward to Treasury's par yields, and the
Board's zero curve is read beside it. Run from anywhere:

    python3 ql_curve.py <responses>

<responses> holds plugin-us-treasury's par-202609.csv.gz and
par-202610.csv.gz and plugin-fed-board's feds200628.csv.gz, as each plugin
recorded them; the file is written beside this folder.
"""
import QuantLib as ql, gzip, csv, io, json, os, sys
D = os.path.join(sys.argv[1] if len(sys.argv) > 1 else '.', '')
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..')
def par_rows():
    rows={}
    for f in ['par-202609.csv.gz','par-202610.csv.gz']:
        r=csv.reader(io.StringIO(gzip.open(D+f).read().decode()))
        head=next(r)
        for row in r:
            m,d,y=row[0].split('/'); rows[f"{y}-{m}-{d}"]=dict(zip(head[1:],row[1:]))
    return rows
TEN={"1 Mo":(1,'M'),"1.5 Month":(6,'W'),"2 Mo":(2,'M'),"3 Mo":(3,'M'),"4 Mo":(4,'M'),"6 Mo":(6,'M'),"1 Yr":(1,'Y'),"2 Yr":(2,'Y'),"3 Yr":(3,'Y'),"5 Yr":(5,'Y'),"7 Yr":(7,'Y'),"10 Yr":(10,'Y'),"20 Yr":(20,'Y'),"30 Yr":(30,'Y')}
U={'M':ql.Months,'W':ql.Weeks,'Y':ql.Years}
def curve(date, pars, kind):
    d=ql.Date(date,'%Y-%m-%d'); ql.Settings.instance().evaluationDate=d
    helpers=[]
    for k,v in pars.items():
        n,u=TEN[k]
        mat=d+ql.Period(n,U[u])
        sched=ql.Schedule(d,mat,ql.Period(ql.Semiannual),ql.NullCalendar(),ql.Unadjusted,ql.Unadjusted,ql.DateGeneration.Backward,False)
        helpers.append(ql.FixedRateBondHelper(ql.QuoteHandle(ql.SimpleQuote(100.0)),0,100.0,sched,[float(v)/100],ql.ActualActual(ql.ActualActual.Bond),ql.Unadjusted))
    c=getattr(ql,kind)(d,helpers,ql.Actual365Fixed())
    c.enableExtrapolation()
    return d,c
out={}
rows=par_rows()
for date in ['2026-10-16','2026-10-01'] if __name__ == '__main__' else []:
    for kind in ['PiecewiseConvexMonotoneZero','PiecewiseConvexMonotoneForward','PiecewiseLogCubicDiscount']:
        d,c=curve(date,rows[date],kind)
        zs={}
        for n,u in [(1,'M'),(6,'M'),(1,'Y'),(2,'Y'),(4,'Y'),(5,'Y'),(8,'Y'),(10,'Y'),(15,'Y'),(20,'Y'),(25,'Y'),(30,'Y')]:
            t=d+ql.Period(n,U[u])
            zs[f"{n}{u}"]=c.zeroRate(t,ql.Actual365Fixed(),ql.Continuous).rate()*100
        out[(date,kind)]=zs
if __name__ == '__main__':
    for k,v in out.items(): print(k, {a:round(b,6) for a,b in v.items()})

# The oracle file: par points, QuantLib's convex monotone forward zeros, and the Board's zero curve.
def gsw(date):
    lines=gzip.open(D+'feds200628.csv.gz').read().decode().splitlines()
    start=[i for i,l in enumerate(lines) if l.startswith('Date,')][0]
    r=csv.reader(io.StringIO("\n".join(lines[start:])))
    head=next(r)
    for row in r:
        if row and row[0]==date:
            m=dict(zip(head,row))
            return {f"{n}Y":m[f"SVENY{n:02d}"] for n in range(1,31)}
if __name__ == '__main__':
    cases=[]
    for date in ['2026-10-16','2026-10-01']:
        d,c=curve(date,rows[date],'PiecewiseConvexMonotoneForward')
        zs={}
        for n,u in [(1,'M'),(6,'M'),(1,'Y'),(2,'Y'),(4,'Y'),(5,'Y'),(8,'Y'),(10,'Y'),(15,'Y'),(20,'Y'),(25,'Y'),(30,'Y')]:
            t=d+ql.Period(n,U[u])
            zs[f"{n}{u}"]=f"{c.zeroRate(t,ql.Actual365Fixed(),ql.Continuous).rate()*100:.10f}"
        cases.append({"date":date,"par":[{"tenor":f"{TEN[k][0]}{TEN[k][1]}","par_yield":v} for k,v in rows[date].items()],
                      "quantlib_convex_monotone_forward_zero":zs,"fed_zero":gsw(date)})
    json.dump({"about":"QuantLib 1.40 PiecewiseConvexMonotoneForward over FixedRateBondHelpers at par (semi-annual, ActualActual Bond, Backward, unadjusted), Actual365Fixed, continuous zero rates in percent; Treasury's par yields and the Board's zero curve (SVENY, continuous, percent) from plugin-us-treasury's and plugin-fed-board's recorded responses","cases":cases},open(os.path.join(OUT,'treasury-curve.json'),'w'),indent=1)
