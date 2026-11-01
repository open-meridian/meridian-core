"""The bond functions oracle (contract v29, W12.3): bond-functions.json.

Test-only tooling, never shipped: QuantLib 1.40's BondFunctions over four
fixed-rate bonds, the z-spread over ql_curve.py's fitted curve. Run as
ql_curve.py is:

    python3 ql_bonds.py <responses>
"""
import QuantLib as ql, json, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ql_curve import curve, par_rows, OUT
rows = par_rows()
VAL='2026-10-16'; SETTLE=ql.Date(19,10,2026)
bonds=[
 {"name":"a corporate, 4.65% to 2046-02-23 on 30/360, at 90.5","security_type":"DEBT_SECURITY_TYPE_CORP","issuer_type":"ISSUER_TYPE_CORPORATE","issue":"2026-02-23","dated":"2026-02-23","first":"","maturity":"2046-02-23","coupon":"4.65","dc":"DAY_COUNT_THIRTY_360_BOND_BASIS","clean":"90.5"},
 {"name":"Treasury's 10-year note, 4.625% to 2036-08-15, at 99.5","security_type":"DEBT_SECURITY_TYPE_TNOTE","issuer_type":"ISSUER_TYPE_SOVEREIGN","issue":"2026-08-17","dated":"2026-08-15","first":"2027-02-15","maturity":"2036-08-15","coupon":"4.625","dc":"DAY_COUNT_ACT_ACT_ICMA","clean":"99.5"},
 {"name":"a municipal, 5% to 2046-05-15 on 30/360, at 104","security_type":"DEBT_SECURITY_TYPE_GO","issuer_type":"ISSUER_TYPE_MUNICIPAL","issue":"2026-05-15","dated":"2026-05-15","first":"2026-11-15","maturity":"2046-05-15","coupon":"5","dc":"DAY_COUNT_THIRTY_360_BOND_BASIS","clean":"104"},
 {"name":"an agency debenture, 4.125% to 2029-03-20 on 30/360, at 98.875","security_type":"DEBT_SECURITY_TYPE_FAC","issuer_type":"ISSUER_TYPE_AGENCY","issue":"2026-03-20","dated":"2026-03-20","first":"2026-09-20","maturity":"2029-03-20","coupon":"4.125","dc":"DAY_COUNT_THIRTY_360_BOND_BASIS","clean":"98.875"},
]
d,curve=curve(VAL,rows[VAL],'PiecewiseConvexMonotoneForward')
ql.Settings.instance().evaluationDate=SETTLE
out=[]
for b in bonds:
    issue=ql.Date(b["dated"],'%Y-%m-%d'); mat=ql.Date(b["maturity"],'%Y-%m-%d')
    dc=ql.Thirty360(ql.Thirty360.BondBasis) if 'THIRTY' in b["dc"] else ql.ActualActual(ql.ActualActual.Bond)
    eom = mat == ql.Date.endOfMonth(mat)
    sched=ql.Schedule(issue,mat,ql.Period(ql.Semiannual),ql.NullCalendar(),ql.Unadjusted,ql.Unadjusted,ql.DateGeneration.Backward,eom)
    bond=ql.FixedRateBond(0,100.0,sched,[float(b["coupon"])/100],dc)
    clean=float(b["clean"])
    y=ql.BondFunctions.bondYield(bond,ql.BondPrice(clean,ql.BondPrice.Clean),dc,ql.Compounded,ql.Semiannual,SETTLE,1e-14,500)
    r=ql.InterestRate(y,dc,ql.Compounded,ql.Semiannual)
    z=ql.BondFunctions.zSpread(bond,ql.BondPrice(clean,ql.BondPrice.Clean),curve,ql.Actual365Fixed(),ql.Compounded,ql.Semiannual,SETTLE,1e-14,500)
    out.append({**b,
      "quantlib":{"accrued":f"{ql.BondFunctions.accruedAmount(bond,SETTLE):.12f}",
        "yield_percent":f"{y*100:.12f}",
        "macaulay":f"{ql.BondFunctions.duration(bond,r,ql.Duration.Macaulay,SETTLE):.12f}",
        "modified":f"{ql.BondFunctions.duration(bond,r,ql.Duration.Modified,SETTLE):.12f}",
        "convexity":f"{ql.BondFunctions.convexity(bond,r,SETTLE):.12f}",
        "bps":f"{ql.BondFunctions.basisPointValue(bond,r,SETTLE):.12f}",
        "z_spread_bp":f"{z*10000:.10f}",
        "clean_at_yield_plus_10bp":f"{ql.BondFunctions.cleanPrice(bond,ql.InterestRate(y+0.001,dc,ql.Compounded,ql.Semiannual),SETTLE):.12f}"}})
json.dump({"about":"QuantLib 1.40 BondFunctions over FixedRateBond (semi-annual, Backward, unadjusted, end of month where maturity is; Thirty360 BondBasis or ActualActual Bond), settled 2026-10-19 for a close on 2026-10-16: yield Compounded Semiannual on the bond's day count; zSpread over PiecewiseConvexMonotoneForward fitted to Treasury's 2026-10-16 par yields, Actual365Fixed, Compounded Semiannual","valuation_date":VAL,"bonds":out},open(os.path.join(OUT,'bond-functions.json'),'w'),indent=1)
for o in out: print(o["name"], o["quantlib"])
