# هندسة Project AEGIS ومطابقة الوثيقة الرسمية

## المبادئ العابرة للطبقات

1. **الحتمية:** الأسعار والكميات `Fixed(i64)` بدقة 8 منازل؛ لا يُستخدم `f64` لتفسير رسائل المنصة. يستخدم `f64` فقط للنسب الإحصائية غير المرسلة كأوامر.
2. **Fail closed:** فقد التسلسل، انتهاء TTL، غياب snapshot bridge، استنفاد rate limit، أو حالة أمر مجهولة توقف الدخول.
3. **لا أسرار في الكود:** جميع الأسرار من environment/Secrets Manager. لا يطبع أي مكون المفاتيح.
4. **تطابق المسار:** `MarketEvent` و`TradeIntent` و`ExecutionVenue` نفسها في live وshadow وbacktest.
5. **المال أولاً:** Shadow افتراضي؛ live يحتاج بوابتين مستقلتين compile-time/runtime.

## Layer 1

- `data/transport.rs`: التدفقات `depth@0ms` و`aggTrade` و`markPrice@1s` و`forceOrder`، heartbeat 2s وexponential backoff.
- `data/parser.rs`: parsing in-place عبر `simd-json` إلى هياكل ثابتة الحجم.
- `data/lob.rs`: جدول أسعار open-addressed ثابت السعة. التحديث O(1) متوسطياً واستخراج top-N بمسح cache-friendly. `LobSynchronizer` يفرض snapshot → bridge → `pu == u السابق`.
- `data/bus.rs`: `rtrb` للمسار SPSC الحتمي و`ArrayQueue::force_push` لسياسة drop-oldest عند الطوفان.
- Snapshot limiter: طلب واحد على الأكثر كل 3 ثوانٍ/رمز.

الـ snapshot وتهيئة URL عمليات باردة مسموح لها بالتخصيص. رسائل WebSocket تحوّل فوراً إلى قيم inline؛ طبقة النقل الحالية تنسخ frame النصي إلى scratch mutable لأن `simd-json` يتطلب buffer قابلاً للتعديل. عند الضبط النهائي يوصى بربط pool buffers بمكتبة النقل وقياس allocations بمخصص عدّاد.

## Layer 2

- OBI لأفضل 5، VPIN بصندوق حجمي وتحديث O(1)، فجوات سعرية وOI delta.
- FSM: `Idle → Armed → Executing → Managing`. يلزم تحقق setup مرتين متتاليتين.
- `CapitalSizer`: `equity × risk / stop-distance` ثم LOT_SIZE/MIN_NOTIONAL/max-notional.
- `AlphaEngine` يحسب gross edge ناقص maker + emergency taker + slippage ويرفض `NEV <= 0`.
- TTL الافتراضي 200ms.

## Layer 3

- `ExecutionVenue`: polymorphism بين `BinanceLiveExecution` و`ShadowExecutionEngine`.
- Live: اتصال دائم إلى `/ws-fapi/v1`، توقيع HMAC-SHA256، `newClientOrderId` ثابت 32 hex، وعدم إعادة إرسال الطلب ذي النتيجة المجهولة.
- `BinanceUserStream`: execution/account events، ping watchdog 3s وتجديد listen key.
- `OrderManager`: GTX للدخول؛ partial-fill abort؛ bracket STOP/TAKE_PROFIT بعد fill؛ transitions متحقق منها؛ IOC محدود 3 ticks؛ orphan cancel/reconcile/flatten.
- `TokenBucket`: احتياطي لا يمكن لأوامر الدخول استهلاكه.
- `OrderSlicer`: أطفال TWAP/VWAP تحت سقف مشاركة السيولة؛ الذيل غير المجدول لا يتعرض تلقائياً.

## Layer 4

- المخاطر نسبة من **live equity**، و0.8 بعد كل خسارة متتالية. تعود القاعدة بعد 3 انتصارات.
- الأصفر: latency ≥50ms أو rejection ≥5%، الحجم ×0.5.
- البرتقالي: drawdown يومي 2%، VPIN مستمر، volatility ×4، أو rolling EV سلبي بعد اكتمال 100 صفقة.
- الأحمر: حركة 3%/10s، USER stream ≥5s، أو drawdown أسبوعي 5%.
- البرتقالي/الأحمر latched؛ الاستئناف يحتاج 15 دقيقة + health check، ثم 25% ويزيد بعد الصفقات الناجحة.
- `TimeWeightedStop` لا يستبدل server hard stop؛ هو منطق خروج إضافي فقط.
- `Telemetry` يستخدم bounded `try_send` وworker UDP. لا تُسجل ticks الخام.

## Layer 5 — المرجع

- Parquet schema محدد في `data-lake-schema.md`.
- `DataFeed` يرفض timestamps العكسية ولا يرتبها لإخفاء عيب التجميع.
- `BacktestEngine` يمرر الأحداث نفسها إلى alpha/OMS/shadow.
- `ShadowExecutionEngine`: queue ahead حقيقي، volume-through، latency PRNG قابل لإعادة الإنتاج، participation cap، maker/taker fees، وتأثير سوق للخروج الفوري.
- `OrderSlicer` ومصفوفة 100/50k/2m يعالجان اختلاف أثر رأس المال.
- Docker/CI/Terraform/systemd تمنع النشر اليدوي غير القابل للتكرار.

## Layer 5 — التحديث الرسمي

- Shadow live يستقبل L2 وtrades الحقيقية ولا يمتلك مفاتيح Binance.
- Telegram task منفصل؛ الرسائل تمر في قناة fire-and-forget وطابور retry محدود دفاعياً.
- whitelist على Telegram User ID؛ `/halt` و`/mode live` يحتاجان رمزاً يومياً محقوناً من قناة منفصلة.
- Telegram لا يستورد ولا يعرف Binance secret.

## خيوط التنفيذ

Tokio يستخدم عدد الأنوية المتاحة. يمكن تثبيت worker المخصص للـ LOB عند تخصيص runtime production عبر `core_affinity`؛ لا يثبت التطبيق كل Tokio workers عشوائياً حتى لا يضع مهمتين على النواة نفسها. يجب قياس tick-to-trade وp99 allocator count على نوع EC2 الفعلي قبل اعتماد pin map.

## معايير الانتقال إلى live

- Unit/integration/loom-style concurrency review ناجح.
- Replay بلا look-ahead وبلا sequence gap غير معالج.
- Walk-forward/OOS وparameter perturbation ناجحة بعد أعلى رسوم.
- Shadow لا يقل عن 30 يوماً ويظهر EV صافياً موجباً مع confidence interval مقبول.
- Chaos tests: network loss، 429/418، stale user stream، partial fills، process kill.
- مراجعة مفاتيح/IAM/IP whitelist وخطة rollback وon-call.
