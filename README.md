# XIX-Upscaler

Aplikasi desktop untuk memperbesar gambar dan video secara batch.

Engine yang tersedia:

- **Image (Online)** — layanan online.
- **Image ESRGAN (Offline)** — diproses di komputer.
- **Video (Colab Experimental)** — memakai GPU Colab dan Google Drive pengguna.

## Menggunakan Video Colab

1. Pilih engine **Video (Colab Experimental)**, video, dan Folder Hasil.
2. Atur target FPS bila diperlukan dan tombol **MUTE** untuk hasil tanpa suara.
3. Tekan **Start**. Login Google akan terbuka jika belum terhubung; tombol **DRIVE** juga dapat dipakai untuk menghubungkan akun.
4. Setelah semua unggahan siap, notebook resmi terbuka. Pilih runtime **GPU**, tekan **Run all**, lalu izinkan Drive menggunakan akun yang sama.
5. Biarkan desktop memantau proses dan menyimpan hasil ke Folder Hasil.

Setiap video dibatasi **100 MB** (104.857.600 byte) dan **1 menit**. Pemeriksaan dilakukan sebelum login atau unggahan. Upscale video 4× memakai NanoVSR; interpolasi memakai RIFE dengan pilihan FPS standar maksimal 60. Mute mati mempertahankan suara jika sumber memiliki suara.

**Pause** meminta jeda di titik aman; status jeda pemrosesan menunggu konfirmasi Colab. Tekan tombol yang sama lagi untuk membatalkan permintaan jeda atau melanjutkan; notebook akan dibuka kembali agar **Run all** dapat dijalankan lagi. **Stop** mengirim pembatalan pekerjaan; saat mengunduh, unduhan lokal dihentikan. Menutup desktop tidak sama dengan Stop: pekerjaan tersimpan dipulihkan saat aplikasi dibuka kembali. Jika perlu login ulang, hubungkan **DRIVE** dan pemulihan berlanjut otomatis. Gunakan **Start** untuk mencoba kembali pekerjaan tersimpan setelah gangguan transfer.

Menu **DRIVE** menyediakan **Ganti akun** dan **Putuskan akun** ketika tidak ada pekerjaan yang sedang berjalan. Pekerjaan lama tetap memerlukan akun asalnya. Jika runtime terputus, tekan **BUKA COLAB**, lalu **Run all** lagi.

Pemulihan hanya melanjutkan pekerjaan yang belum selesai. Catatan pekerjaan yang sudah selesai tidak dimasukkan kembali ke antrean, meskipun hasil lamanya sudah dipindahkan atau dihapus. Pilih video yang sama dan tekan **Start** untuk memprosesnya lagi; hasil baru tidak menimpa hasil sebelumnya. Jika ada pekerjaan tertunda akibat gangguan transfer, **Start** tetap melanjutkannya terlebih dahulu. Baris pekerjaan dipisahkan berdasarkan identitas pekerjaan, sehingga dua percobaan untuk sumber yang sama tidak saling menimpa status.

Hasil diperiksa sebelum dinyatakan selesai dan tidak menimpa hasil lama. File kerja di Drive dipindahkan ke Sampah setelah hasil lokal terverifikasi, kecuali **Simpan file kerja di Drive** diaktifkan pada ADV. File input lokal tidak dihapus. Pembatalan atau kegagalan tidak menghapus file kerja di Drive secara otomatis.

Notebook publik: [XIX-Upscaler Colab](https://colab.research.google.com/github/mfahryf/xix-upscaler-colab/blob/main/XIX-Upscaler-Colab.ipynb).
Panduan dan checklist pengujian ada di `../colab/`. Fitur masih Experimental; login Google dan proses GPU Colab nyata perlu diuji manual sebelum dibagikan umum.

## Development

Jalankan mode development dari folder ini dengan:

```powershell
npm install
npm run dev
```

Untuk melihat jalur koneksi OAuth, Google Drive, upload, dan notebook di terminal:

```powershell
npm run dev:debug
```

Cari baris berawalan `[COLAB-DEBUG]`. Log ini tidak menampilkan token, kode OAuth,
alamat email lengkap, path lokal, atau URL sesi Drive.

Untuk pengujian OAuth lokal, aplikasi membaca `.env` di folder aplikasi. Isinya hanya
digunakan di komputer lokal dan tidak boleh dimasukkan ke Git:

```text
XIX_GOOGLE_CLIENT_ID=client_id_dari_Google
XIX_GOOGLE_CLIENT_SECRET=client_secret_dari_Google
```

File `.env` otomatis diabaikan Git. Untuk lokasi lain, gunakan variabel
`XIX_GOOGLE_ENV_FILE` yang menunjuk ke file tersebut.

Perintah di atas hanya panduan untuk pengujian manual; pengembangan integrasi ini tidak menjalankan aplikasi atau membangun installer.
