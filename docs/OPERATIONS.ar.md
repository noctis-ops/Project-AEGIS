# دليل تشغيل AEGIS

## 1. التسلسل الإلزامي

1. `cargo test --all-targets`.
2. backtest على 100 و50,000 و2,000,000 USDT.
3. تقسيم walk-forward: 70% تدريب و30% اختبار أعمى، ثم معاملات مجاورة.
4. Shadow على السوق الحقيقي 30 يوماً على الأقل.
5. مراجعة نتائج الرسوم والانزلاق والـ fill ratio والـ queue wait والـ p99 latency.
6. فحص أمني، حساب Binance منفصل، مفتاح Futures فقط بلا withdrawals مع IP whitelist.
7. بدء live بمخاطرة أدنى من الإعداد الافتراضي وتواجد مهندس on-call.

## 2. أسرار التشغيل

في AWS تُخزن القيم في Secrets Manager وتحقن إلى `/etc/aegis/aegis.env` بصلاحية `0600`. رمز C2 اليومي يولد خارج Telegram وخارج حاوية AEGIS. لا تضع القيم في Terraform variables أو user-data أو logs.

## 3. التشغيل الشبحي

```bash
sudo systemctl start aegis
journalctl -u aegis -f
```

افحص `/status`. تأكد أن الوضع `Shadow`، السوق متصل، والرصيد افتراضي. انقطاع market stream يلغي الأوامر الوهمية ويعيد snapshot.

## 4. الإطلاق الحقيقي

ابنِ artifact مع `live-trading`. لا تستخدم صورة shadow قديمة. عيّن:

```text
AEGIS_LIVE_TRADING=I_UNDERSTAND_THE_RISK
BINANCE_API_KEY=<Secrets Manager>
BINANCE_SECRET_KEY=<Secrets Manager>
```

غيّر `ExecStart` إلى `live BTCUSDT` عبر إصدار IaC جديد، لا عبر تعديل يدوي على الخادم. راقب أول أمر حتى وصول fill ثم وجود STOP_MARKET وTAKE_PROFIT_MARKET في Binance.

## 5. الاستجابة للحوادث

### Market stream stale أو sequence gap

يدخل النظام resync، يوقف الدخول، يلغي الأوامر المفتوحة ويجلب snapshot. إذا لم تعد المزامنة فلا تستخدم `/resume`؛ أصلح الشبكة أولاً.

### USER_DATA_STREAM stale

المسار آلي: Red → cancel all → GET positionRisk → flatten. افحص حساب Binance مباشرة، ثم السجلات. لا تستأنف قبل 15 دقيقة وفحص الصحة.

### 429/418

أبقِ النظام متوقفاً. لا تغيّر IP للتحايل. افحص response weights ومعدل إعادة التسعير، وانتظر مدة الحظر الرسمية. احتياطي token bucket مخصص للإغلاق فقط.

### أمر يتيم/حالة مجهولة

لا تعِد إرسال order.place. ابحث بـ `clientOrderId` أو شغّل reconciliation، ثم سوِّ التعرض غير المطابق.

### Telegram compromise

عطّل token من BotFather، دوّر الرمز اليومي، وأوقف الخدمة عبر AWS SSM. Telegram لا يملك مفتاح Binance، لكن افحص سجل أوامر C2.

## 6. الاستعادة

- systemd يعيد العملية تلقائياً.
- عند البدء الحقيقي يجب أن يعمل user stream ثم ground-truth reconciliation قبل السماح بإشارة جديدة.
- Orange/Red latched؛ `/resume` لا يتجاوز cooldown أو health check.
- يبدأ الاستئناف بـ25% ويرتفع بعد 5 نتائج ناجحة.

## 7. المقاييس والتنبيهات

StatsD exporter على UDP 8125 يحول القياسات إلى Prometheus. Grafana تعرض:

- tick-to-trade وorder RTT p50/p95/p99؛
- equity، exposure، gross/net PnL؛
- slippage والرسوم؛
- rejection وsequence gaps وbus drops؛
- breaker level وuser/market stream health.

لا ترسل raw order-book ticks إلى logging أو Telegram.
