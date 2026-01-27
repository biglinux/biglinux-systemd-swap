#!/bin/bash

################################################################################
# Script de Preparação para Atualização do systemd-swap
# Remove configurações antigas antes de instalar o pacote otimizado
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

ask_confirmation() {
    local question="$1"
    echo -e "\n${YELLOW}${BOLD}$question${NC} ${WHITE}(s/N)${NC}: \c"
    read -r response
    case "$response" in
        [sS][iI][mM]|[sS])
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

check_root() {
    if [ "$EUID" -ne 0 ]; then
        print_error "Este script precisa ser executado como root (sudo)"
        exit 1
    fi
}

################################################################################
# Banner
################################################################################

clear
echo -e "${BOLD}${CYAN}"
cat << "EOF"
   ____            _                     ____
  / ___| _   _ ___| |_ ___ _ __ ___   __/ ___|_      ____ _ _ __
  \___ \| | | / __| __/ _ \ '_ ` _ \ / _\___ \ \ /\ / / _` | '_ \
   ___) | |_| \__ \ ||  __/ | | | | | |_ ___) \ V  V / (_| | |_) |
  |____/ \__, |___/\__\___|_| |_| |_|\__|____/ \_/\_/ \__,_| .__/
         |___/                                              |_|

            🚀 Preparação para Atualização Otimizada 🚀
EOF
echo -e "${NC}"

print_info "Este script irá preparar seu sistema para a atualização otimizada"
print_info "do systemd-swap com configurações para máxima fluidez."
echo ""

################################################################################
# Verificações iniciais
################################################################################

check_root

print_header "1️⃣  VERIFICAÇÕES INICIAIS"

# Verificar se o serviço existe
if systemctl list-unit-files | grep -q "systemd-swap.service"; then
    print_success "Serviço systemd-swap encontrado"

    # Status do serviço
    if systemctl is-active --quiet systemd-swap; then
        print_info "Serviço está ATIVO"
        SERVICE_ACTIVE=1
    else
        print_warning "Serviço está INATIVO"
        SERVICE_ACTIVE=0
    fi
else
    print_error "Serviço systemd-swap NÃO encontrado"
    exit 1
fi

# Verificar uso atual de swap
SWAP_TOTAL=$(free -h | awk '/^Swap:/ {print $2}')
SWAP_USED=$(free -h | awk '/^Swap:/ {print $3}')
echo ""
print_info "Swap atual: ${BOLD}${SWAP_USED}${NC} de ${BOLD}${SWAP_TOTAL}${NC} em uso"

# Verificar arquivos que serão removidos
echo ""
print_step "Arquivos/diretórios que serão removidos:"
FILES_TO_REMOVE=(
    "/etc/systemd/swap.conf"
    "/etc/systemd/swap.conf.old"
    "/etc/systemd/swap.conf.d"
    "/etc/sysctl.d/99-swappiness.conf"
)

for file in "${FILES_TO_REMOVE[@]}"; do
    if [ -e "$file" ]; then
        print_warning "Encontrado: $file"
    else
        print_info "Não existe: $file"
    fi
done

# Verificar swap files
if [ -d "/swapfc" ] && [ "$(ls -A /swapfc 2>/dev/null)" ]; then
    SWAPFC_SIZE=$(du -sh /swapfc 2>/dev/null | cut -f1)
    print_warning "Diretório /swapfc existe (${SWAPFC_SIZE})"
    CLEAN_SWAPFC=1
else
    print_info "Diretório /swapfc vazio ou não existe"
    CLEAN_SWAPFC=0
fi

################################################################################
# Confirmação do usuário
################################################################################

echo ""
if ! ask_confirmation "Deseja continuar com a limpeza?"; then
    print_error "Operação cancelada pelo usuário"
    exit 0
fi

################################################################################
# Fase 1: Parar serviço e desabilitar swap
################################################################################

print_header "2️⃣  PARANDO SERVIÇO E DESABILITANDO SWAP"

if [ $SERVICE_ACTIVE -eq 1 ]; then
    print_step "Parando systemd-swap..."
    if systemctl stop systemd-swap 2>/dev/null; then
        print_success "Serviço parado com sucesso"
    else
        print_error "Falha ao parar o serviço"
        exit 1
    fi
else
    print_info "Serviço já estava parado"
fi

print_step "Desabilitando todo swap ativo..."
if swapoff -a 2>/dev/null; then
    print_success "Swap desabilitado com sucesso"
else
    print_warning "Swap já estava desabilitado ou erro ao desabilitar"
fi

sleep 1

################################################################################
# Fase 2: Remover configurações antigas
################################################################################

print_header "3️⃣  REMOVENDO CONFIGURAÇÕES ANTIGAS"

for file in "${FILES_TO_REMOVE[@]}"; do
    if [ -e "$file" ]; then
        print_step "Removendo: $file"
        if rm -rf "$file" 2>/dev/null; then
            print_success "Removido com sucesso"
        else
            print_error "Falha ao remover"
        fi
    fi
done

################################################################################
# Fase 3: Limpar swap files (opcional)
################################################################################

if [ $CLEAN_SWAPFC -eq 1 ]; then
    echo ""
    if ask_confirmation "Deseja limpar os arquivos de swap antigos em /swapfc? (Libera ${SWAPFC_SIZE})"; then
        print_header "4️⃣  LIMPANDO ARQUIVOS DE SWAP"

        print_step "Removendo arquivos em /swapfc..."
        if rm -rf /swapfc/* 2>/dev/null; then
            print_success "Arquivos removidos com sucesso (${SWAPFC_SIZE} liberados)"
        else
            print_warning "Falha ao remover alguns arquivos"
        fi
    else
        print_info "Mantendo arquivos de swap existentes"
    fi
fi

################################################################################
# Resumo
################################################################################

print_header "✅ LIMPEZA CONCLUÍDA"

print_success "Sistema preparado para atualização!"
echo ""
print_info "Próximos passos:"
echo ""
echo -e "  ${CYAN}1.${NC} Instale o novo pacote systemd-swap otimizado"
echo -e "     ${BOLD}sudo pacman -S systemd-swap${NC}  (ou yay, makepkg, etc)"
echo ""
echo -e "  ${CYAN}2.${NC} Execute o script de pós-instalação:"
echo -e "     ${BOLD}sudo ./post-install.sh${NC}"
echo ""

print_warning "NÃO reinicie o serviço manualmente ainda!"
print_warning "Use o script post-install.sh após instalar o pacote."
echo ""
