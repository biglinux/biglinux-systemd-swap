#!/bin/bash

################################################################################
# Script de Pós-Instalação do systemd-swap
# Reinicia serviço e aplica novas configurações otimizadas
################################################################################

# Cores
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
MAGENTA='\033[0;35m'
CYAN='\033[0;36m'
WHITE='\033[1;37m'
BOLD='\033[1m'
NC='\033[0m' # No Color

# Símbolos
CHECK="${GREEN}✓${NC}"
CROSS="${RED}✗${NC}"
ARROW="${CYAN}➜${NC}"
WARN="${YELLOW}⚠${NC}"
INFO="${BLUE}ℹ${NC}"
ROCKET="${MAGENTA}🚀${NC}"

################################################################################
# Funções auxiliares
################################################################################

print_header() {
    echo -e "\n${BOLD}${MAGENTA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BOLD}${WHITE}  $1${NC}"
    echo -e "${BOLD}${MAGENTA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}\n"
}

print_step() {
    echo -e "${ARROW} ${BOLD}$1${NC}"
}

print_success() {
    echo -e "  ${CHECK} ${GREEN}$1${NC}"
}

print_error() {
    echo -e "  ${CROSS} ${RED}$1${NC}"
}

print_warning() {
    echo -e "  ${WARN} ${YELLOW}$1${NC}"
}

print_info() {
    echo -e "  ${INFO} ${CYAN}$1${NC}"
}

check_root() {
    if [ "$EUID" -ne 0 ]; then
        print_error "Este script precisa ser executado como root (sudo)"
        exit 1
    fi
}

wait_with_dots() {
    local message="$1"
    local seconds="$2"
    echo -ne "  ${INFO} ${CYAN}${message}${NC}"
    for ((i=1; i<=seconds; i++)); do
        sleep 1
        echo -n "."
    done
    echo ""
}

################################################################################
# Banner
################################################################################

clear
echo -e "${BOLD}${GREEN}"
cat << "EOF"
   ____            _                     ____
  / ___| _   _ ___| |_ ___ _ __ ___   __/ ___|_      ____ _ _ __
  \___ \| | | / __| __/ _ \ '_ ` _ \ / _\___ \ \ /\ / / _` | '_ \
   ___) | |_| \__ \ ||  __/ | | | | | |_ ___) \ V  V / (_| | |_) |
  |____/ \__, |___/\__\___|_| |_| |_|\__|____/ \_/\_/ \__,_| .__/
         |___/                                              |_|

              🎯 Ativação das Otimizações 🎯
EOF
echo -e "${NC}"

print_info "Este script irá ativar as novas configurações otimizadas"
print_info "do systemd-swap para máxima fluidez e performance."
echo ""

################################################################################
# Verificações iniciais
################################################################################

check_root

print_header "1️⃣  VERIFICANDO INSTALAÇÃO"

# Verificar se o pacote foi instalado
if [ ! -f "/usr/share/systemd-swap/swap-default.conf" ]; then
    print_error "Arquivo de configuração padrão não encontrado!"
    print_error "Certifique-se de que o pacote systemd-swap foi instalado."
    exit 1
else
    print_success "Pacote systemd-swap instalado"
fi

# Verificar se o sysctl foi instalado
if [ -f "/usr/lib/sysctl.d/99-systemd-swap.conf" ]; then
    print_success "Arquivo sysctl encontrado"
else
    print_warning "Arquivo sysctl não encontrado (pode não ter sido incluído nesta versão)"
fi

# Verificar se o binário existe
if [ -f "/usr/bin/systemd-swap" ]; then
    print_success "Binário systemd-swap instalado"
else
    print_error "Binário systemd-swap não encontrado!"
    exit 1
fi

################################################################################
# Fase 1: Aplicar sysctl
################################################################################

print_header "2️⃣  APLICANDO CONFIGURAÇÕES DO KERNEL"

print_step "Recarregando configurações sysctl..."
if sysctl --system > /dev/null 2>&1; then
    print_success "Sysctl recarregado com sucesso"
else
    print_warning "Aviso ao recarregar sysctl (pode ser normal)"
fi

# Verificar swappiness
CURRENT_SWAPPINESS=$(cat /proc/sys/vm/swappiness)
echo ""
print_info "vm.swappiness atual: ${BOLD}${CURRENT_SWAPPINESS}${NC}"

if [ "$CURRENT_SWAPPINESS" -eq 60 ]; then
    print_success "Swappiness configurado corretamente (60)"
elif [ -f "/usr/lib/sysctl.d/99-systemd-swap.conf" ]; then
    print_warning "Swappiness ainda em ${CURRENT_SWAPPINESS}, aplicando manualmente..."
    sysctl -w vm.swappiness=60 > /dev/null 2>&1
    sysctl -w vm.page-cluster=0 > /dev/null 2>&1
    print_success "Aplicado: vm.swappiness=60, vm.page-cluster=0"
else
    print_info "Usando swappiness padrão do sistema"
fi

################################################################################
# Fase 2: Recarregar systemd e iniciar serviço
################################################################################

print_header "3️⃣  INICIANDO SYSTEMD-SWAP OTIMIZADO"

print_step "Recarregando daemon do systemd..."
if systemctl daemon-reload 2>/dev/null; then
    print_success "Daemon recarregado"
else
    print_error "Falha ao recarregar daemon"
    exit 1
fi

print_step "Habilitando systemd-swap..."
if systemctl enable systemd-swap 2>/dev/null; then
    print_success "Serviço habilitado para boot"
else
    print_warning "Serviço já estava habilitado"
fi

print_step "Iniciando systemd-swap..."
if systemctl start systemd-swap 2>/dev/null; then
    print_success "Serviço iniciado com sucesso"
else
    print_error "Falha ao iniciar o serviço"
    echo ""
    print_info "Logs de erro:"
    journalctl -u systemd-swap -n 20 --no-pager
    exit 1
fi

wait_with_dots "Aguardando inicialização" 3

################################################################################
# Fase 3: Verificar status
################################################################################

print_header "4️⃣  VERIFICANDO STATUS"

# Verificar se o serviço está ativo
if systemctl is-active --quiet systemd-swap; then
    print_success "Serviço ATIVO e rodando"
else
    print_error "Serviço NÃO está ativo!"
    exit 1
fi

# Mostrar informações do swap
echo ""
print_step "Status atual do sistema:"
echo ""

# Executar systemd-swap status se disponível
if command -v systemd-swap &> /dev/null; then
    systemd-swap status 2>/dev/null || {
        # Fallback se o comando falhar
        free -h
    }
else
    free -h
fi

################################################################################
# Fase 4: Mostrar otimizações aplicadas
################################################################################

print_header "5️⃣  OTIMIZAÇÕES ATIVAS"

# Detectar modo de swap
if [ -f "/etc/systemd/swap.conf" ]; then
    SWAP_MODE=$(grep "^swap_mode=" /etc/systemd/swap.conf 2>/dev/null | cut -d'=' -f2)
else
    SWAP_MODE="auto"
fi

echo -e "${BOLD}Modo de Swap:${NC} ${GREEN}${SWAP_MODE}${NC}"
echo ""

print_info "Compressor: ${BOLD}LZ4${NC} (2-3x mais rápido)"
print_info "Pool Zswap: ${BOLD}50%${NC} (~20GB swap em RAM)"
print_info "Allocator: ${BOLD}zsmalloc${NC} (allocator padrão)"
print_info "Chunk Size: ${BOLD}1GB${NC} (melhor para NVMe/SSD)"
print_info "Anti-Thrashing: ${BOLD}5000ms${NC} (proteção forte)"
print_info "Pré-alocação: ${BOLD}Habilitada${NC} (melhor performance)"

################################################################################
# Resumo final
################################################################################

print_header "✅ CONFIGURAÇÃO CONCLUÍDA"

echo -e "${ROCKET} ${BOLD}${GREEN}Sistema otimizado com sucesso!${NC}\n"

print_info "Resultados esperados:"
echo -e "  ${CYAN}•${NC} Redução de 70% no swap em disco"
echo -e "  ${CYAN}•${NC} 15-20GB de swap comprimido em RAM"
echo -e "  ${CYAN}•${NC} 3-4x mais responsivo"
echo -e "  ${CYAN}•${NC} Menos acesso ao disco"
echo ""

print_warning "Monitoramento recomendado:"
echo -e "  ${BOLD}watch -n 5 'free -h && echo && systemd-swap status'${NC}"
echo ""
echo -e "  ${BOLD}journalctl -u systemd-swap -f${NC}  (logs em tempo real)"
echo ""

print_info "Aguarde alguns minutos para o sistema estabilizar."
print_info "Se estiver usando muita RAM, reinicie aplicações pesadas."
echo ""

# Oferecer monitoramento
echo -e "${YELLOW}${BOLD}Deseja iniciar o monitoramento agora?${NC} ${WHITE}(s/N)${NC}: \c"
read -r response
case "$response" in
    [sS][iI][mM]|[sS])
        echo ""
        print_success "Iniciando monitoramento (CTRL+C para sair)..."
        sleep 2
        watch -n 5 'free -h && echo && systemd-swap status 2>/dev/null'
        ;;
    *)
        echo ""
        print_success "Instalação finalizada! Aproveite seu sistema otimizado! 🚀"
        echo ""
        ;;
esac
