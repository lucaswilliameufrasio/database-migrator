# Disposable PostgreSQL, MySQL, and MariaDB on the Windows GitHub runner.
# Service containers are Linux-only. The archives are the official zip
# builds, started on 127.0.0.1. Windows MySQL cannot use
# lower_case_table_names=0, so these servers keep the Windows default.
$ErrorActionPreference = 'Stop'

$root = Join-Path $env:RUNNER_TEMP 'dbm-windows-servers'
if (Test-Path $root) {
    Remove-Item -Recurse -Force $root
}
New-Item -ItemType Directory -Path $root | Out-Null

function Get-Archive([string]$Url, [string]$Name) {
    $zip = Join-Path $root "$Name.zip"
    $dest = Join-Path $root $Name
    Write-Host "downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $zip
    Expand-Archive -Path $zip -DestinationPath $dest
    return $dest
}

function Wait-Tcp([int]$Port, [string]$LogPath) {
    $attempt = 1
    while ($attempt -le 90) {
        $client = $null
        try {
            $client = New-Object System.Net.Sockets.TcpClient
            $client.Connect('127.0.0.1', $Port)
            return
        } catch {
            Start-Sleep -Seconds 2
            $attempt += 1
        } finally {
            if ($null -ne $client) {
                $client.Dispose()
            }
        }
    }
    Write-Host "timed out waiting for 127.0.0.1:$Port"
    if (Test-Path $LogPath) {
        Get-Content $LogPath -Tail 50
    }
    throw "server on port $Port did not accept connections"
}

function Find-Exe([string]$Directory, [string]$FileName) {
    $found = Get-ChildItem -Path $Directory -Filter $FileName -Recurse -File | Select-Object -First 1
    if ($null -eq $found) {
        throw "missing $FileName under $Directory"
    }
    return $found.FullName
}

$pgDest = Get-Archive 'https://get.enterprisedb.com/postgresql/postgresql-18.6-1-windows-x64-binaries.zip' 'postgres'
$mysqlDest = Get-Archive 'https://cdn.mysql.com/Downloads/MySQL-8.4/mysql-8.4.13-winx64.zip' 'mysql'
$mariaDest = Get-Archive 'https://archive.mariadb.org/mariadb-11.8.9/winx64-packages/mariadb-11.8.9-winx64.zip' 'mariadb'

$initdb = Find-Exe $pgDest 'initdb.exe'
$pgCtl = Find-Exe $pgDest 'pg_ctl.exe'
$psql = Find-Exe $pgDest 'psql.exe'
$mysqld = Find-Exe $mysqlDest 'mysqld.exe'
$mysql = Find-Exe $mysqlDest 'mysql.exe'
$mariaInstall = Get-ChildItem -Path $mariaDest -Recurse -File | Where-Object { $_.Name -eq 'mysql_install_db.exe' -or $_.Name -eq 'mariadb-install-db.exe' } | Select-Object -First 1
if ($null -eq $mariaInstall) {
    throw 'MariaDB bootstrap executable was not in the archive'
}
$mariaInstall = $mariaInstall.FullName
$mariadbd = $null
foreach ($candidate in @('mariadbd.exe', 'mysqld.exe')) {
    $path = Get-ChildItem -Path $mariaDest -Filter $candidate -Recurse -File | Select-Object -First 1
    if ($null -ne $path) {
        $mariadbd = $path.FullName
        break
    }
}
if ($null -eq $mariadbd) {
    throw 'MariaDB server executable was not in the archive'
}
$mariadb = Find-Exe $mariaDest 'mysql.exe'

$pgData = Join-Path $root 'pg-data'
$mysqlData = Join-Path $root 'mysql-data'
$mariaData = Join-Path $root 'maria-data'
New-Item -ItemType Directory -Path $mariaData | Out-Null
$mysqlTmp = Join-Path $root 'mysql-tmp'
$mariaTmp = Join-Path $root 'maria-tmp'
New-Item -ItemType Directory -Path $mysqlTmp, $mariaTmp | Out-Null

& $initdb -D $pgData -U dbm -A trust -E UTF8 --locale=C
if ($LASTEXITCODE -ne 0) {
    throw 'initdb failed'
}
& $pgCtl -D $pgData -l (Join-Path $root 'postgres.log') -o '-c listen_addresses=127.0.0.1 -c port=5432' start
if ($LASTEXITCODE -ne 0) {
    throw 'pg_ctl start failed'
}

function Write-RootTcpInit([string]$Path) {
    # initialize-insecure creates root@localhost. TCP to 127.0.0.1 does not
    # match that account when skip-name-resolve is on, so the server creates
    # the TCP account before it accepts clients.
    $sql = "CREATE USER IF NOT EXISTS 'root'@'127.0.0.1' IDENTIFIED BY 'dbm';`nGRANT ALL PRIVILEGES ON *.* TO 'root'@'127.0.0.1' WITH GRANT OPTION;`n"
    $utf8 = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText($Path, $sql, $utf8)
    return ($Path -replace '\\', '/')
}

$mysqlLog = Join-Path $root 'mysql.err'
$mysqlInit = Start-Process -FilePath $mysqld -ArgumentList @(
    '--no-defaults',
    "--datadir=$mysqlData",
    "--tmpdir=$mysqlTmp",
    '--initialize-insecure',
    "--log-error=$mysqlLog"
) -Wait -PassThru -NoNewWindow
if ($mysqlInit.ExitCode -ne 0) {
    if (Test-Path $mysqlLog) {
        Get-Content $mysqlLog -Tail 50
    }
    throw 'mysql initialize failed'
}
$mysqlInitSql = Write-RootTcpInit (Join-Path $root 'mysql-init.sql')
$mysqlProc = Start-Process -FilePath $mysqld -ArgumentList @(
    '--no-defaults',
    "--datadir=$mysqlData",
    "--tmpdir=$mysqlTmp",
    "--init-file=$mysqlInitSql",
    '--bind-address=127.0.0.1',
    '--port=3306',
    '--skip-name-resolve',
    "--log-error=$mysqlLog"
) -PassThru -WindowStyle Hidden

$mariaLog = Join-Path $root 'mariadb.err'
$mariaInit = Start-Process -FilePath $mariaInstall -ArgumentList @(
    '--datadir', $mariaData
) -Wait -PassThru -NoNewWindow
if ($mariaInit.ExitCode -ne 0) {
    throw 'mariadb install db failed'
}
$mariaInitSql = Write-RootTcpInit (Join-Path $root 'mariadb-init.sql')
$mariaProc = Start-Process -FilePath $mariadbd -ArgumentList @(
    '--no-defaults',
    "--datadir=$mariaData",
    "--tmpdir=$mariaTmp",
    "--init-file=$mariaInitSql",
    '--bind-address=127.0.0.1',
    '--port=3307',
    '--skip-name-resolve',
    "--log-error=$mariaLog"
) -PassThru -WindowStyle Hidden

try {
    Wait-Tcp 5432 (Join-Path $root 'postgres.log')
    Wait-Tcp 3306 $mysqlLog
    Wait-Tcp 3307 $mariaLog

    & $psql -h 127.0.0.1 -p 5432 -U dbm -d postgres -c 'CREATE DATABASE dbm;'
    if ($LASTEXITCODE -ne 0) {
        throw 'postgres create database failed'
    }

    $mysqlSql = @"
CREATE DATABASE dbm CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
CREATE USER 'dbm'@'127.0.0.1' IDENTIFIED BY 'dbm';
GRANT ALL PRIVILEGES ON *.* TO 'dbm'@'127.0.0.1' WITH GRANT OPTION;
FLUSH PRIVILEGES;
"@
    $mysqlSql | & $mysql --protocol=TCP -h 127.0.0.1 -P 3306 -uroot -pdbm
    if ($LASTEXITCODE -ne 0) {
        if (Test-Path $mysqlLog) {
            Get-Content $mysqlLog -Tail 80
        }
        throw 'mysql user setup failed'
    }
    $mysqlSql | & $mariadb --protocol=TCP -h 127.0.0.1 -P 3307 -uroot -pdbm
    if ($LASTEXITCODE -ne 0) {
        if (Test-Path $mariaLog) {
            Get-Content $mariaLog -Tail 80
        }
        throw 'mariadb user setup failed'
    }

    $pgVersion = & $psql -h 127.0.0.1 -p 5432 -U dbm -d dbm -Atc 'SHOW server_version'
    $mysqlVersion = & $mysql --protocol=TCP -h 127.0.0.1 -P 3306 -udbm -pdbm -N -e 'SELECT VERSION()'
    $mariaVersion = & $mariadb --protocol=TCP -h 127.0.0.1 -P 3307 -udbm -pdbm -N -e 'SELECT VERSION()'
    Write-Host "postgres_version=$pgVersion"
    Write-Host "mysql_version=$mysqlVersion"
    Write-Host "mariadb_version=$mariaVersion"

    $env:DBM_TEST_DATABASE_URL = 'postgres://dbm@127.0.0.1:5432/dbm?sslmode=disable'
    $env:DBM_TEST_MYSQL_URL = 'mysql://dbm:dbm@127.0.0.1:3306/dbm'
    $env:DBM_TEST_MARIADB_URL = 'mysql://dbm:dbm@127.0.0.1:3307/dbm'
    cargo test --workspace --all-features --locked -- --ignored --skip pgbouncer --skip _tls_ --test-threads=1
    if ($LASTEXITCODE -ne 0) {
        throw 'windows server suite failed'
    }
} finally {
    & $pgCtl -D $pgData stop | Out-Null
    if ($null -ne $mysqlProc -and -not $mysqlProc.HasExited) {
        Stop-Process -Id $mysqlProc.Id -Force
    }
    if ($null -ne $mariaProc -and -not $mariaProc.HasExited) {
        Stop-Process -Id $mariaProc.Id -Force
    }
}
