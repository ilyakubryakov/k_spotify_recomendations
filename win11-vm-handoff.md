# Задача: проверить Windows-VM `win11-pro` на этой станции

Ты работаешь на Linux-станции (CachyOS, хост `vika-main`, юзер `k3lmiir`).
На ней в KVM/libvirt живёт Windows 11 Pro VM. Её надо запустить, подключиться и
прогнать смоук-тесты. VM фактически не загружалась с 28 июля 2026, так что часть
вещей могла отвалиться - это и надо выяснить.

Работай через SSH. RDP не трогай: он интерактивный, для человека.

---

## 0. БЛОКИРУЮЩАЯ ПРЕДПОСЫЛКА - проверь первым делом

Станция могла остаться на старом ядре после обновления. Тогда не грузится модуль
`bridge`, не поднимается `virbr0`, и VM не стартует вообще.

```bash
uname -r
ls -1 /usr/lib/modules/
```

Если каталога с именем работающего ядра нет в `/usr/lib/modules/` - **останавливайся и
скажи человеку, что нужен ребут**. Сам не перезагружай станцию. Симптом, который ты
увидишь дальше, если полезешь напролом:

```
error: Failed to start network default
error: Unable to create bridge virbr0: Package not installed
```

«Package not installed» тут - это ENOPKG от ядра про отсутствующий модуль, а НЕ про
pacman-пакет. Не пытайся ничего доустанавливать через pacman, это ложный след.

Если каталог на месте - иди дальше.

---

## 1. Поднять сеть и VM

Все `virsh` идут к системному демону, URI обязателен: `qemu:///system`.
Без `-c qemu:///system` попадёшь в пользовательскую сессию, где доменов нет.

```bash
# сеть default (autostart стоит, но после сбоя её надо поднять руками)
virsh -c qemu:///system net-list --all
virsh -c qemu:///system net-start default   # если inactive
ip -br addr show virbr0                      # ожидаем 192.168.122.1/24

# домен
virsh -c qemu:///system list --all
virsh -c qemu:///system start win11-pro
```

Автостарта у домена нет (`Autostart: disable`) - это намеренно, не включай.

Загрузка Windows занимает до ~90 секунд. Дождись порта:

```bash
for i in $(seq 1 90); do
  timeout 1 bash -c 'echo > /dev/tcp/192.168.122.100/22' 2>/dev/null && { echo SSH_UP; break; }
  sleep 1
done
```

---

## 2. Подключение

| Что | Значение |
|---|---|
| IP | `192.168.122.100` (DHCP-резервация на MAC `52:54:00:77:11:00`) |
| Юзер | `k3lmiir` (локальный админ) |
| Ключ | `~/.ssh/win11_vm` |
| Имя гостя | `win-virt` |
| Профиль | `C:\Users\k3lmi` (имя усечено! в скриптах всегда `%USERPROFILE%` / `$env:USERPROFILE`, никогда не хардкодь `k3lmiir`) |

```bash
ssh -i ~/.ssh/win11_vm k3lmiir@192.168.122.100 'whoami; hostname'
```

Записи в `~/.ssh/config` для этой VM НЕТ - ключ указывай флагом каждый раз.

Сессия приходит elevated (High integrity), ключ лежит в
`C:\ProgramData\ssh\administrators_authorized_keys` - так и задумано для админской учётки.

**Пароль тебе не нужен.** Он есть только в `~/.local/bin/win11-rdp.sh` для RDP-кнопки
человека. Не читай его, не логируй и не вставляй в отчёт.

### Грабли автоматизации по SSH (проверено ранее, не переоткрывай)

- `powershell 5.1 -Command -` со stdin работает - это твой рабочий путь.
- `winget install` и `pwsh` ЛОМАЮТСЯ внутри `powershell -Command -` со stdin (упираются
  в VT/консоль). Если надо winget - клади команды в `.cmd`, копируй через `scp` и
  запускай файлом.
- PS7-модули ставятся не через `pwsh`, а из рабочего 5.1:
  `Save-Module -Path "$HOME\Documents\PowerShell\Modules"`.
- PS7 в госте - это Store/MSIX-версия (алиас в WindowsApps).

---

## 3. Смоук-тесты

Прогони по порядку, фиксируй по каждому пункту: **OK / сломано / не проверено**.

**3.1 Базовое**
```
hostname; whoami
systeminfo | findstr /B /C:"OS Name" /C:"OS Version"
```
Ожидаем Win11 Pro build 26200 (25H2).

**3.2 Сеть гостя**
```powershell
ipconfig /all
Test-NetConnection 8.8.8.8 -Port 443
Resolve-DnsName google.com
Test-NetConnection 192.168.50.101 -Port 445
```
Если DNS или исходящий TCP молчат - корень почти всегда UFW на хосте, а не гость.
Проверь на хосте `sudo ufw status | grep virbr0`, должны быть все четыре правила:
53/udp, 53/tcp, 67/udp на virbr0 плюс `ALLOW FWD ... on virbr0`.
Диагностический признак: **ping идёт, а TCP/HTTPS висят** = FORWARD-политика режет
новые исходящие. На момент написания все правила на месте.
Не гоняйся за offload на tap (`ethtool -K vnetN tx off`) - это проверенный ложный след.
И помни: Windows Firewall режет входящий ICMP, так что `ping` с хоста в гостя не
проходит даже на живой VM. Проверяй L2 через `arping -I virbr0 192.168.122.100`.

**3.3 Шара virtiofs - диск Z:**
```powershell
Get-Service VirtioFsSvc
Get-ChildItem Z:\ | Select-Object -First 5
```
Z: = `/home/k3lmiir` хоста, проброшен через virtiofs (WinFsp + viofs-драйвер).
Если сервис есть, а диска нет - смотри сервис, не переустанавливай драйвер.

**3.4 SMB-шары с fileserver - Y: и X:**
```powershell
net use
Get-ChildItem Y:\ | Select-Object -First 5
```
`Y:` = archive, `X:` = media с `192.168.50.101`, юзер `ilya`.
Маппит `mount-shares.cmd` из Startup-папки при ИНТЕРАКТИВНОМ логине.
**Важно:** в SSH-сессии (network logon) этих дисков может не быть штатно - это не баг.
Если пусто, проверь наличие самого скрипта:
`Get-ChildItem "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Startup"`.
Пароль к шаре - в 1Password, запись «fileserver SMB share (ilya)». Сам не доставай,
если понадобится - попроси человека.

**3.5 Активация и обновления**
```powershell
cscript //nologo C:\Windows\System32\slmgr.vbs /dli
Get-HotFix | Sort-Object InstalledOn -Descending | Select-Object -First 5
```
Windows активирована цифровой лицензией. Если слетела - это находка, доложи, сам не чини.
Учти: VM стояла без обновлений с конца июля, большой пакет WU - ожидаемо, не поломка.

**3.6 Софт**
```powershell
Get-Command git, oh-my-posh, bat, eza, rg, fd, fzf, zoxide, jq, yq -ErrorAction SilentlyContinue |
  Select-Object Name, Version
```
Плюс проверь, что Teams и Outlook (new) на месте - это единственное, ради чего VM сейчас живёт:
```powershell
Get-AppxPackage | Where-Object Name -match 'Teams|Outlook' | Select-Object Name, Version
```

**3.7 Профиль PowerShell**
```powershell
Test-Path $PROFILE
Get-ChildItem "$env:USERPROFILE\Documents\PowerShell"
```
Должен быть паритет с zsh: алиасы `k`/`g`/`ll`/`la`/`l`, zoxide, PSReadLine,
тема oh-my-posh `kanagawa.omp.json` рядом с профилем.

---

## 4. Когда закончишь

```bash
virsh -c qemu:///system shutdown win11-pro
```
Мягкий shutdown. `destroy` не используй - это дёргание кабеля питания.

---

## Правила и границы

- **Ничего не чини молча.** Задача - диагностика. Нашёл поломку - опиши и спроси.
- **Не перезагружай станцию** и не меняй её конфиг.
- **Не трогай Defender, Windows Update и Windows Firewall в госте** - они намеренно
  не тронуты при деблоате.
- Не включай автостарт домена, не правь XML домена, не меняй ufw.
- Пароли и секреты не читай, не логируй, не вставляй в отчёт.
- Не ставь ClamAV / OPSWAT на ХОСТ, если такая мысль возникнет по ходу:
  `OnAccessPrevention yes` в clamd вешал эту станцию намертво. Compliance на станции
  уже закрыт нативно SentinelOne.
- Не запускай GUI-софт ради проверки - человек тестирует GUI сам.

## Отчёт

Таблица: пункт -> OK / сломано / не проверено -> короткая заметка.
Отдельно списком: что требует решения человека.
