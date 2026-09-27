# Project AEGIS

تنفيذ Rust للوثيقة الرسمية `Project_AEGIS.md`: محرك تداول/محاكاة لعقود Binance USD-M الدائمة، مبني على خمس طبقات، ويعمل افتراضياً في **الوضع الشبحي فقط**.

> **تحذير مالي:** هذا برنامج هندسي تجريبي وليس ضماناً للربح أو نصيحة استثمارية. لا تُفعّل التنفيذ الحقيقي قبل مراجعة مستقلة، اختبارات تاريخية وخارج العينة، تداول شبحي طويل، وتدقيق أمني وتشغيلي.

## ما تم بناؤه

| الطبقة | التنفيذ |
|---|---|
| 1 — البيانات | WebSocket متعدد التدفقات، نبضات ومصالحة تلقائية، `simd-json`، أرقام ثابتة الدقة، دفتر L2 ثابت السعة، بروتوكول snapshot/`U-u-pu`، ناقل أحدث-بيانات lock-free وحدّ snapshot محلي |
| 2 — Alpha | OBI لأفضل 5 مستويات، VPIN بصناديق حجمية، فجوات السيولة، دلتا OI، FSM بإشارة مستمرة، NEV بعد الرسوم والانزلاق، وحجم مركز متكيف مع الرصيد/التذبذب |
| 3 — التنفيذ | واجهة `ExecutionVenue` متعددة الأشكال، GTX maker-first، IOC محدود الانزلاق، FSM للأوامر الجزئية، HMAC، WebSocket Order API، USER_DATA_STREAM، فلاتر المنصة، قاطع معدل محلي، bracket server-side وحماية الأوامر اليتيمة |
| 4 — المخاطر | تخميد anti-martingale، MAE/MFE، EV متحرك، قواطع أخضر/أصفر/برتقالي/أحمر، funding/volatility guards، تبريد 15 دقيقة واستئناف عند 25%، وقياس UDP غير حاجب |
| 5 — المحاكاة والنشر | Parquet data lake، تغذية زمنية سببية، backtest حدثي، كمون حتمي 2–10ms، نموذج queue/volume-through، رسوم وتأثير سوق متشائمان، shadow engine على السوق الحي، Telegram C2 عربي، Docker وCI وTerraform/systemd |

## حدود الأمان المتعمدة

- الوضع الافتراضي `shadow` لا يرسل أي أمر إلى Binance.
- التنفيذ الحقيقي غير موجود في الـ binary إلا عند البناء بميزة `live-trading`.
- حتى بعد ذلك يلزم `AEGIS_LIVE_TRADING=I_UNDERSTAND_THE_RISK` ومفاتيح محقونة وقت التشغيل.
- أي انقطاع في `USER_DATA_STREAM` يفعّل الأحمر، يلغي الأوامر، يجلب حقيقة المراكز ثم يسويها.
- تغيير `/mode live` في Telegram **لا** يبدّل المحرك أثناء التشغيل؛ يسجل طلب إعادة تشغيل آمنة فقط.
- لا توجد أسرار أو ملفات `.env` داخل المستودع.

## البدء السريع

### المتطلبات

- Rust stable (MSRV المعلن 1.85؛ بيئة البناء المثبتة 1.98.1)
- Linux موصى به للإنتاج
- بيانات Parquet وفق [`docs/data-lake-schema.md`](docs/data-lake-schema.md) للاختبار التاريخي

```bash
cargo test --all-targets
cargo run --release -- shadow BTCUSDT
```

يستقبل الوضع الشبحي دفتر السوق الحقيقي لكنه يحتفظ برصيد ومراكز وهمية محلياً. يرفض المحاكي «التعبئة باللمس»؛ يجب أن يستهلك التداول الحقيقي الحجم الموجود أمام الأمر.

### Backtest

```bash
cargo run --release --features parquet-data -- backtest data/BTCUSDT.parquet 1000
```

شغّل مصفوفة رأس المال المطلوبة على الملف نفسه:

```bash
for equity in 100 50000 2000000; do
  cargo run --release --features parquet-data -- backtest data/BTCUSDT.parquet "$equity"
done
```

يجب مقارنة 70% in-sample و30% out-of-sample، ثم إعادة الاختبار حول كل معامل (مثل OBI 0.40/0.45/0.50). لا تعتمد نتيجة بلا بيانات خارج العينة.

### Telegram C2

انسخ أسماء المتغيرات فقط؛ لا تحفظ القيم في Git:

```bash
export TELEGRAM_BOT_TOKEN='runtime-secret'
export TELEGRAM_ALLOWED_USER_ID='123456789'
export TELEGRAM_NOTIFICATION_CHAT_ID='123456789'
export AEGIS_C2_CONFIRMATION_CODE='daily-code-from-separate-secure-channel'
cargo run --release -- shadow BTCUSDT
```

الأوامر: `/status`، `/halt` (تأكيد ثانٍ)، `/resume`، `/risk 0.5`، `/mode shadow|live`. تُقبل الرسائل حصراً من User ID المدرج في القائمة البيضاء.

### التنفيذ الحقيقي

اقرأ [`docs/OPERATIONS.ar.md`](docs/OPERATIONS.ar.md) كاملاً. البناء والتشغيل المقصودان متعمدان وغير مريحين:

```bash
cargo build --release --features 'live-trading parquet-data'
export BINANCE_API_KEY='injected-by-secrets-manager'
export BINANCE_SECRET_KEY='injected-by-secrets-manager'
export AEGIS_LIVE_TRADING='I_UNDERSTAND_THE_RISK'
./target/release/project-aegis live BTCUSDT
```

استخدم مفتاح Futures فقط، بلا صلاحية سحب، ومقيداً بعنوان Elastic IP للخادم. ابدأ بحساب منفصل وأقل مخاطرة ممكنة.

## هيكل المستودع

```text
src/data/          Layer 1
src/alpha/         Layer 2
src/execution/     Layer 3
src/risk/          Layer 4
src/simulation/    Layer 5 reference implementation
src/c2.rs          Layer 5 official update (Telegram C2)
infra/terraform/   AWS Tokyo IaC
ops/               systemd, Prometheus/StatsD/Grafana
.github/workflows/ CI and controlled deployment
```

## أوامر الجودة

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo test --all-targets --features parquet-data
```

التصميم التفصيلي وخرائط المتطلبات موجودة في [`docs/ARCHITECTURE.ar.md`](docs/ARCHITECTURE.ar.md). الوثيقة الرسمية الأصلية محفوظة دون تعديل في `Project_AEGIS.md`.
