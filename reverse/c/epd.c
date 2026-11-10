/* epd.c —— reMarkable 2 自主 EPD 驱动
 *
 * 不依赖 xochitl / SWTCON / 任何预生成的波形表：自己解析
 * /usr/share/remarkable 与 /var/lib/uboot 下的 .wbf 文件，按面板指纹选文件，
 * 按温度选档，自己编码 32x32 波形 LUT 并逐帧喂给 LCDIF。
 *
 * 关键约束（违反任何一条都会导致"白屏/残影"）：
 *   1. 每个波形相位必须恰好占一个帧周期(11.76 ms)。编码必须 < 一帧，
 *      否则预编码。写 fb 必须整字 32 位，且绝不能对被扫描的槽做非整字访问。
 *   2. 每次更新序列结束后必须再喂一帧"全 0(无驱动)"，否则 LCDIF 会
 *      永远重复最后一帧，把前面所有相位覆盖掉。
 *   3. prv(上一帧状态) 每次更新后都要同步，否则不变区域会被判为"无变化"。
 *
 * 用法：
 *   epd info                          列出所有波形文件与选择结果
 *   epd init                          上电初始化（官方 INIT 序列 + 空白帧）
 *   epd test                          画内置测试图（页面 + 16 级灰阶）
 *   epd draw <file.pgm> [mode]        画 PGM(P5, 1404x1872)
 *   epd local <x> <y> <w> <h> <g> <n> 局部刷新一个矩形
 *   epd blank                         喂一帧全 0
 *   mode: gc16(默认) du a2 du4
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>
#include <errno.h>
#include <dirent.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <linux/fb.h>

/* ------------------------------------------------------------------ 常量 */
#define CELLS   260           /* 每行 cell 数（含 26 个命令 cell） */
#define LINE    (CELLS*4)     /* 1040 B */
#define ROWS    1408          /* 4 preamble + 1404 面板列 */
#define SLOT    ((size_t)LINE*ROWS)
#define UW      1404          /* 用户宽 */
#define UH      1872          /* 用户高 */
#define COLS    1404          /* 面板列数 */
#define CSTR    234           /* 内容 cell 数 = 1872/8 */
#define PDROW   (CSTR*2)
#define NCELL   26            /* 内容 cell 起始下标 */
#define PIXCLK  0x7080
#define LOWMARG 143
#define STATES  32            /* 5-bit LUT */
#define BYTES_PER_PHASE (STATES*STATES/4)   /* 256 */

/* 本机已验证的面板指纹。来源：官方 xochitl 自己的日志
 *   Reading waveforms from /usr/share/remarkable/
 *     320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC.wbf
 * 官方是从 /dev/mmcblk2boot1 的 TLV 记录推出来的；本机该分区的 5 条记录
 * （RM110-111-01171 / FUM211004939 / BYD210114001810 / EUFZBRBC1W1V009UBAT -1.33
 *   / TESTS_DONE）里没有字面量 355，说明官方走了回退路径，映射规则未解。
 * 这里直接用已验证结果，并允许 /etc/rm2epd.conf 或环境变量覆盖。 */
#define DEFAULT_FPL_LOT 0x163u        /* 355 -> R355 */
#define DEFAULT_TFT_VID "ED103TC2U2"

#define STATE_FILE "/tmp/rm2epd.state"
#define CONF_FILE  "/etc/rm2epd.conf"

static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e3+t.tv_nsec/1e6;}

/* ------------------------------------------------------------------- fb */
static int fb_fd=-1; static unsigned char *fbmem; static struct fb_var_screeninfo vi;

static int fb_open(void){
    fb_fd=open("/dev/fb0",O_RDWR|O_CLOEXEC);
    if(fb_fd<0){perror("open /dev/fb0");return -1;}
    if(ioctl(fb_fd,FBIOGET_VSCREENINFO,&vi)){perror("FBIOGET_VSCREENINFO");return -1;}
    fbmem=mmap(0,SLOT*2,PROT_READ|PROT_WRITE,MAP_SHARED,fb_fd,0);
    if(fbmem==MAP_FAILED){perror("mmap fb");return -1;}
    return 0;
}
static int fb_setmode(void){
    struct fb_var_screeninfo v; memset(&v,0,sizeof v);
    if(ioctl(fb_fd,FBIOGET_VSCREENINFO,&v)){perror("getvar");return -1;}
    v.xres=CELLS; v.yres=ROWS; v.xres_virtual=CELLS; v.yres_virtual=ROWS*2;
    v.xoffset=0; v.yoffset=0; v.bits_per_pixel=32;
    v.red.offset=16; v.red.length=8; v.green.offset=8; v.green.length=8; v.blue.offset=0; v.blue.length=8;
    v.pixclock=PIXCLK;
    v.left_margin=1; v.right_margin=1; v.upper_margin=1; v.lower_margin=LOWMARG;
    v.hsync_len=1; v.vsync_len=1;
    v.sync=0; v.vmode=FB_VMODE_NONINTERLACED; v.activate=FB_ACTIVATE_NOW;
    if(ioctl(fb_fd,FBIOPUT_VSCREENINFO,&v)){perror("FBIOPUT_VSCREENINFO");return -1;}
    if(ioctl(fb_fd,FBIOGET_VSCREENINFO,&vi)){perror("getvar2");return -1;}
    return 0;
}
static int pan0(void){ vi.yoffset=0; return ioctl(fb_fd,FBIOPAN_DISPLAY,&vi); }

/* ------------------------------------------------------------- 命令模板 */
static void fill_first_line(uint32_t *l){
    for(int i=0;i<CELLS;i++) l[i]=0x00430000u;
    for(int i=20;i<143;i++) l[i]|=0x00040000u;
    for(int i=40;i<103;i++) l[i]&=0xFFFDFFFFu;
}
static void fill_line(uint32_t *l,int content){
    for(int i=0;i<CELLS;i++) l[i]=0x00410000u;
    for(int i=8;i<19;i++)   l[i]|=0x00200000u;
    for(int i=55;i<255;i++) l[i]|=0x00020000u;
    if(content) for(int i=NCELL;i<CELLS;i++) l[i]|=0x00100000u;
}

/* ------------------------------------------------------------- wbf 解析 */
static unsigned rd_u24(const unsigned char *p){return p[0]|(p[1]<<8)|(p[2]<<16);}
static unsigned rd_u16(const unsigned char *p){return p[0]|(p[1]<<8);}
static unsigned rd_u32(const unsigned char *p){return p[0]|(p[1]<<8)|(p[2]<<16)|((unsigned)p[3]<<24);}

typedef struct {
    char path[512];
    unsigned char *buf; size_t size;
    /* header */
    unsigned fpl_lot, mode_version, wf_version, wf_subversion, luts, mc, trc, frame_rate, vcom_offset;
    unsigned nmode, nband;
    unsigned char bounds[65];
    unsigned xwia, wmta, mode_table_off;
    char xwi[128];
    char tft_vid[48];
} wbf_t;

static int read_file(const char *p, unsigned char **out, size_t *sz){
    int fd=open(p,O_RDONLY|O_CLOEXEC);
    if(fd<0) return -1;
    off_t n=lseek(fd,0,SEEK_END); lseek(fd,0,SEEK_SET);
    if(n<=0||n>(32<<20)){close(fd);return -2;}
    unsigned char *b=malloc((size_t)n);
    if(!b){close(fd);return -3;}
    if(read(fd,b,(size_t)n)!=(ssize_t)n){free(b);close(fd);return -4;}
    close(fd);
    *out=b; *sz=(size_t)n; return 0;
}

/* xwi 里取出 tft_vid：形如 320_R259_AFAB21_ED103TC2M1_TC */
static void parse_tft_vid(const char *xwi, char *out, size_t n){
    out[0]=0;
    const char *f[8]; int nf=0;
    const char *p=xwi;
    while(*p && nf<8){
        f[nf++]=p;
        const char *u=strchr(p,'_');
        if(!u) break;
        p=u+1;
    }
    for(int i=0;i<nf;i++){
        const char *e=(i+1<nf)?(f[i+1]-1):(f[i]+strlen(f[i]));
        size_t len=(size_t)(e-f[i]);
        if(len>=2 && f[i][0]=='A' && f[i][1]=='F' && i+1<nf){
            const char *s=f[i+1];
            const char *e2=(i+2<nf)?(f[i+2]-1):(s+strlen(s));
            size_t l2=(size_t)(e2-s); if(l2>=n) l2=n-1;
            memcpy(out,s,l2); out[l2]=0; return;
        }
    }
    for(int i=0;i<nf;i++){
        const char *e=(i+1<nf)?(f[i+1]-1):(f[i]+strlen(f[i]));
        size_t len=(size_t)(e-f[i]);
        if(len>=2 && f[i][0]=='E' && f[i][1]=='D'){
            if(len>=n) len=n-1;
            memcpy(out,f[i],len); out[len]=0; return;
        }
    }
}

static int wbf_open(const char *path, wbf_t *w){
    memset(w,0,sizeof *w);
    snprintf(w->path,sizeof w->path,"%s",path);
    int r=read_file(path,&w->buf,&w->size);
    if(r) return r;
    const unsigned char *b=w->buf;
    if(w->size<64) return -5;
    w->fpl_lot       = rd_u16(b+0x0E);
    w->mode_version  = b[0x10];
    w->wf_version    = b[0x11];
    w->wf_subversion = b[0x12];
    w->frame_rate    = b[0x17];
    w->vcom_offset   = b[0x19];
    w->xwia          = rd_u24(b+0x1C);
    w->wmta          = rd_u24(b+0x20);
    w->luts          = b[0x24];
    w->mc            = b[0x25];
    w->trc           = b[0x26];
    w->nmode = w->mc+1;
    w->nband = w->trc+1;
    if(w->nmode>64||w->nband>64||w->nmode==0||w->nband==0) return -6;
    if(w->luts!=0x04) return -7;                  /* 只支持 5-bit 双向 LUT */
    if((size_t)(48+w->nband+2)>w->size) return -8;
    memcpy(w->bounds,b+48,w->nband+1);
    /* xwi */
    if(w->xwia && w->xwia<w->size){
        unsigned ln=b[w->xwia];
        if(w->xwia+1+ln<=w->size && ln<sizeof w->xwi){
            memcpy(w->xwi,b+w->xwia+1,ln); w->xwi[ln]=0;
            parse_tft_vid(w->xwi,w->tft_vid,sizeof w->tft_vid);
        }
    }
    /* 模式表偏移 */
    unsigned computed=48+w->nband+2;
    if(w->xwi[0]) computed+=1+(unsigned)strlen(w->xwi)+1;
    w->mode_table_off = w->wmta ? w->wmta : computed;
    if(!(w->mode_table_off>0 && (size_t)w->mode_table_off+4*w->nmode<=w->size))
        w->mode_table_off=computed;
    if((size_t)w->mode_table_off+4*w->nmode>w->size) return -9;
    return 0;
}
static void wbf_close(wbf_t *w){ free(w->buf); w->buf=0; }

/* 模式名表（mode_version -> 名字序列），只为按名字找 GC16/DU/A2/DU4 */
static const char *mode_name(unsigned mv,int i){
    static const struct { unsigned mv; const char *n[8]; } T[]={
        {0x01,{"INIT","DU","GL8","GC8",0}},
        {0x03,{"INIT","DU","GL16","GL16","A2",0}},
        {0x09,{"INIT","DU","GC16","A2",0}},
        {0x12,{"INIT","DU","NULL","GC16","A2","GL16","GL16","DU4"}},
        {0x15,{"INIT","DU","GC16","GL16","A2","DU4","GC4",0}},
        {0x16,{"INIT","DU","GC16","GL16","GL16","GC16","A2",0}},
        {0x18,{"INIT","DU","GC16","GL16","GL16","GL16","A2",0}},
        {0x19,{"INIT","DU","GC16","GL16","GL16","GL16","A2","DU4"}},
        {0x20,{"INIT","DU","GC16","GL16","GL16","GL16","A2",0}},
        {0x23,{"INIT","DU","GC16","GL16","A2","DU4",0}},
        {0x43,{"INIT","DU","GC16","GL16","GL16","GL16","A2","DU4"}},
        {0x54,{"INIT","DU","GC16","GL16","GL16","A2",0}},
        {0x48,{"INIT","DU","GC16","GL16","GL16","NULL","A2",0}},
        {0x83,{"INIT","DU","GC16","GL16","A2","GL16","GL16",0}},
    };
    for(size_t k=0;k<sizeof T/sizeof T[0];k++)
        if(T[k].mv==mv) return T[k].n[i];
    return 0;
}
static int mode_index(const wbf_t *w, const char *want, int nth){
    for(unsigned i=0;i<w->nmode;i++){
        const char *n=mode_name(w->mode_version,i);
        if(n && !strcmp(n,want)){ if(nth--==0) return (int)i; }
    }
    return -1;
}

/* 取某 (模式,温度档) 的波形表地址 */
static unsigned table_addr(const wbf_t *w,int mode,int band){
    const unsigned char *b=w->buf;
    size_t off=(size_t)w->mode_table_off+4*mode;
    if(off+4>w->size) return 0;
    unsigned base=rd_u24(b+off);
    if(base>=w->size) return 0;
    off=(size_t)base+4*band;
    if(off+4>w->size) return 0;
    return rd_u24(b+off);
}

/* RLE 展开一张波形表。返回展开长度(字节)，失败返回 0。 */
static size_t decode_table(const wbf_t *w, unsigned addr, unsigned char **out){
    const unsigned char *b=w->buf; size_t n=w->size, p=addr;
    size_t cap=BYTES_PER_PHASE*64, len=0;
    unsigned char *o=malloc(cap);
    if(!o) return 0;
    int rle=1; unsigned cs=0;
    for(;;){
        if(p>=n){ free(o); return 0; }
        unsigned c=b[p++]; cs=(cs+c)&0xFF;
        if(c==0xFC){ rle=!rle; }
        else if(c==0xFF){
            if(p<n && b[p]!=cs){ /* 校验和不符，仍返回数据但计数打折扣 */ }
            *out=o; return len;
        }
        else if(!rle){ if(len>=cap){cap*=2;o=realloc(o,cap);} o[len++]=(unsigned char)c; }
        else {
            if(p>=n) break;
            unsigned ln=b[p++]; cs=(cs+ln)&0xFF;
            for(unsigned k=0;k<=ln;k++){ if(len>=cap){cap*=2;o=realloc(o,cap); if(!o) return 0;} o[len++]=(unsigned char)c; }
        }
        if(len>cap){ cap*=2; unsigned char *t=realloc(o,cap); if(!t){free(o);return 0;} o=t; }
    }
    free(o);
    return 0;
}

/* -------------------------------------------------------- 面板指纹与选文件 */
typedef struct {
    char path[512];
    unsigned fpl_lot; char tft_vid[48]; unsigned mode_version;
    int usable;
} cand_t;

static int scan_dir(const char *dir, cand_t *out, int max, int n){
    DIR *d=opendir(dir); if(!d) return n;
    struct dirent *e;
    while((e=readdir(d)) && n<max){
        size_t l=strlen(e->d_name);
        if(l<4) continue;
        const char *x=e->d_name+l-4;
        if(strcasecmp(x,".wbf")!=0) continue;
        char p[512]; snprintf(p,sizeof p,"%s/%s",dir,e->d_name);
        wbf_t w;
        if(wbf_open(p,&w)){ wbf_close(&w); continue; }
        snprintf(out[n].path,sizeof out[n].path,"%s",p);
        out[n].fpl_lot=w.fpl_lot;
        snprintf(out[n].tft_vid,sizeof out[n].tft_vid,"%s",w.tft_vid);
        out[n].mode_version=w.mode_version;
        out[n].usable=1;
        wbf_close(&w);
        n++;
    }
    closedir(d);
    return n;
}

static int read_conf(unsigned *fpl, char *tft, size_t tn){
    FILE *f=fopen(CONF_FILE,"r"); if(!f) return -1;
    char line[256]; int got=0;
    while(fgets(line,sizeof line,f)){
        char *eq=strchr(line,'='); if(!eq) continue;
        *eq=0; char *k=line, *v=eq+1;
        while(*k==' '||*k=='\t')k++;
        while(*v==' '||*v=='\t')v++;
        char *en=v+strlen(v); while(en>v&&(en[-1]=='\n'||en[-1]=='\r'||en[-1]==' ')) *--en=0;
        if(!strcmp(k,"fpl_lot")){ *fpl=(unsigned)strtoul(v,0,0); got|=1; }
        else if(!strcmp(k,"tft_vid")){ snprintf(tft,tn,"%s",v); got|=2; }
    }
    fclose(f);
    return got;
}

/* 选一个 wbf：优先显式路径/环境变量/配置，其次指纹匹配(fpl_lot+tft_vid)，
   再次只用 fpl_lot / 只用 tft_vid，最后任选一个（并大声警告）。 */
static int pick_wbf(char *path, size_t pn, unsigned *want_fpl, char *want_tft,
                    size_t tn, int verbose){
    const char *env=getenv("RM2EPD_WBF");
    if(env && *env){ snprintf(path,pn,"%s",env); return 1; }
    unsigned ef=0; char et[48]={0};
    const char *e1=getenv("RM2EPD_FPL_LOT"), *e2=getenv("RM2EPD_TFT_VID");
    if(e1&&*e1) ef=(unsigned)strtoul(e1,0,0);
    if(e2&&*e2) snprintf(et,sizeof et,"%s",e2);
    unsigned cf=0; char ct[48]={0};
    int cg=read_conf(&cf,ct,sizeof ct);
    if(!ef && (cg&1)) ef=cf;
    if(!et[0] && (cg&2)) snprintf(et,sizeof et,"%s",ct);
    if(!ef){ ef=DEFAULT_FPL_LOT; }
    if(!et[0]){ snprintf(et,sizeof et,"%s",DEFAULT_TFT_VID); }
    *want_fpl=ef; snprintf(want_tft,tn,"%s",et);

    static cand_t c[256];
    int n=0;
    n=scan_dir("/usr/share/remarkable",c,256,n);
    n=scan_dir("/var/lib/uboot",c,256,n);
    if(verbose){
        printf("%-58s %-6s %-14s mv\n","文件","FPL","TFT_VID");
        for(int i=0;i<n;i++)
            printf("%-58s 0x%02X   %-14s 0x%02X\n",c[i].path,c[i].fpl_lot,
                   c[i].tft_vid[0]?c[i].tft_vid:"?",c[i].mode_version);
        printf("共 %d 个波形文件\n",n);
    }
    if(n==0) return 0;
    int best=-1, why=0;
    if(ef && et[0]){
        for(int i=0;i<n;i++) if(c[i].fpl_lot==ef && !strcmp(c[i].tft_vid,et)){ best=i;why=1;break; }
        if(best<0){ if(verbose) printf("提示: 没有同时匹配 FPL_LOT=0x%02X 与 TFT_VID=%s 的文件\n",ef,et); }
    }
    if(best<0 && ef){
        for(int i=0;i<n;i++) if(c[i].fpl_lot==ef){ best=i;why=2;break; }
        if(best>=0&&verbose) printf("提示: 回退——只按 FPL_LOT=0x%02X 匹配\n",ef);
    }
    if(best<0 && et[0]){
        for(int i=0;i<n;i++) if(!strcmp(c[i].tft_vid,et)){ best=i;why=3;break; }
        if(best>=0&&verbose) printf("提示: 回退——只按 TFT_VID=%s 匹配\n",et);
    }
    if(best<0){ best=0; why=4;
        if(verbose) printf("警告: 没有面板指纹，任选一个文件（可能不是本机的波形）\n"); }
    snprintf(path,pn,"%s",c[best].path);
    if(verbose){
        static const char *whyS[]={"","精确匹配 FPL_LOT+TFT_VID","仅 FPL_LOT","仅 TFT_VID","无指纹(任选)"};
        printf("=> 选中: %s  (%s)\n",path,whyS[why]);
    }
    return 1;
}

/* --------------------------------------------------------------- 渲染器 */
static uint32_t *slab;            /* RAM 模板槽 */
static unsigned char *img;        /* 目标图像 x-major: img[x*UH+y] 0..15 */
static unsigned char *prv;        /* 上一帧状态，同布局 */
static unsigned char *kp;         /* key = (tgt<<4)|prv */

static void slab_init(void){
    if(slab) return;
    slab=malloc(SLOT);
    if(!slab){fprintf(stderr,"OOM slab\n");exit(1);}
    fill_first_line(slab+0*CELLS);
    fill_line(slab+1*CELLS,0);
    fill_line(slab+2*CELLS,0);
    fill_line(slab+3*CELLS,1);
    for(int r=4;r<ROWS;r++)
        for(int i=0;i<CELLS;i++) slab[(size_t)r*CELLS+i]=slab[(size_t)3*CELLS+i];
}
static void slab_zero_one_col(int pc){
    uint32_t *l=slab+(size_t)(4+pc)*CELLS;
    for(int s=0;s<CSTR;s++) l[NCELL+s]&=0xFFFF0000u;
}
static void slab_put_one_col(int pc,const uint16_t *o){
    uint32_t *l=slab+(size_t)(4+pc)*CELLS;
    for(int s=0;s<CSTR;s++) l[NCELL+s]=(l[NCELL+s]&0xFFFF0000u)|o[s];
}
static void slab_zero_all(void){ for(int pc=0;pc<COLS;pc++) slab_zero_one_col(pc); }
static void slab_flush(void){
    const uint32_t *s=slab; uint32_t *d=(uint32_t*)fbmem;
    for(size_t i=0;i<SLOT/4;i++) d[i]=s[i];   /* 必须整字 32 位 */
}
static void prep(const unsigned char *src,unsigned char T[256]){
    for(int gc=0;gc<16;gc++) for(int gp=0;gp<16;gp++){
        int idx=((gc*2)<<5)|(gp*2);
        T[(gc<<4)|gp]=(unsigned char)((src[idx>>2]>>((idx&3)*2))&3);
    }
}
static void build_kp(void){
    size_t n=(size_t)UW*UH;
    for(size_t i=0;i<n;i++) kp[i]=(unsigned char)(((img[i]&0xF)<<4)|(prv[i]&0xF));
}
static void encode_phase(const unsigned char T[256],uint16_t *out){
    for(int pc=0;pc<COLS;pc++){
        uint16_t *o=out+(size_t)pc*CSTR;
        const unsigned char *k=kp+(size_t)(COLS-1-pc)*UH;
        for(int s=0;s<CSTR;s++){
            const unsigned char *b=k+(UH-1-s*8)-7;
            o[s]=(uint16_t)( (unsigned)T[b[0]]      | ((unsigned)T[b[1]]<<2)
                           | ((unsigned)T[b[2]]<<4) | ((unsigned)T[b[3]]<<6)
                           | ((unsigned)T[b[4]]<<8) | ((unsigned)T[b[5]]<<10)
                           | ((unsigned)T[b[6]]<<12)| ((unsigned)T[b[7]]<<14) );
        }
    }
}
/* 只编码面板列 [pclo,pchi]，结果写进 out（按列索引整块、只用这些列会被写） */
static void encode_cols(const unsigned char T[256],uint16_t *out,int pclo,int pchi){
    for(int pc=pclo;pc<=pchi;pc++){
        uint16_t *o=out+(size_t)pc*CSTR;
        const unsigned char *k=kp+(size_t)(COLS-1-pc)*UH;
        for(int s=0;s<CSTR;s++){
            const unsigned char *b=k+(UH-1-s*8)-7;
            o[s]=(uint16_t)( (unsigned)T[b[0]]      | ((unsigned)T[b[1]]<<2)
                           | ((unsigned)T[b[2]]<<4) | ((unsigned)T[b[3]]<<6)
                           | ((unsigned)T[b[4]]<<8) | ((unsigned)T[b[5]]<<10)
                           | ((unsigned)T[b[6]]<<12)| ((unsigned)T[b[7]]<<14) );
        }
    }
}
/* 显示一帧整槽数据并停一个帧周期 */
static void show_plane(const uint16_t *one){
    for(int pc=0;pc<COLS;pc++) slab_put_one_col(pc,one+(size_t)pc*CSTR);
    slab_flush(); pan0();
}
/* 全 0 无驱动帧——每次更新结束必须调用 */
static void show_blank(void){ slab_zero_all(); slab_flush(); pan0(); }

/* -------------------------------------------------------------- 初始化 */
static void do_init(double *ms){
    /* 官方 INIT：双极性、净零驱动，111 帧，结束时为白色。
       这是实测通过的序列，比按 wbf INIT 表逐相位编码更稳。 */
    static const int seq[]={1,24,0,4,2,24,0,5,1,24,0,4,2,24,0,2};
    static uint16_t one[COLS*CSTR];
    double t0=now();
    for(int k=0;k<16;k+=2){
        uint16_t v=(seq[k]==0)?0u:((seq[k]==1)?0x5555u:0xAAAAu);
        for(int i=0;i<COLS*CSTR;i++) one[i]=v;
        for(int n=0;n<seq[k+1];n++) show_plane(one);
    }
    show_blank();
    *ms=now()-t0;
}

/* --------------------------------------------------------- 全屏 GC16/等 */
static int render_full(const wbf_t *w,const char *mname,int band,double *enc_ms,double *play_ms){
    int mi=mode_index(w,mname,0);
    if(mi<0){ fprintf(stderr,"波形里没有 %s 模式\n",mname); return -1; }
    unsigned addr=table_addr(w,mi,band);
    unsigned char *tab=0; size_t len=decode_table(w,addr,&tab);
    if(!len){ fprintf(stderr,"波形表解码失败 (mode=%s band=%d addr=0x%X)\n",mname,band,addr); return -1; }
    int nph=(int)(len/BYTES_PER_PHASE);
    size_t per=(size_t)COLS*CSTR;
    uint16_t *pd=malloc((size_t)nph*per*2);
    if(!pd){ fprintf(stderr,"OOM phases (%d x %zu B)\n",nph,per*2); free(tab); return -1; }
    build_kp();
    double t0=now();
    unsigned char T[256];
    for(int ph=0;ph<nph;ph++){ prep(tab+(size_t)ph*BYTES_PER_PHASE,T); encode_phase(T,pd+(size_t)ph*per); }
    *enc_ms=now()-t0;
    t0=now();
    for(int ph=0;ph<nph;ph++) show_plane(pd+(size_t)ph*per);
    *play_ms=now()-t0;
    show_blank();
    free(pd); free(tab);
    return nph;
}

/* -------------------------------------------------------------- 状态文件 */
static int load_state(void){
    int fd=open(STATE_FILE,O_RDONLY|O_CLOEXEC);
    if(fd<0) return -1;
    size_t want=(size_t)UW*UH, got=0;
    while(got<want){ ssize_t r=read(fd,prv+got,want-got); if(r<=0) break; got+=(size_t)r; }
    close(fd);
    return got==want?0:-1;
}
static void save_state(void){
    int fd=open(STATE_FILE,O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC,0644);
    if(fd<0) return;
    size_t want=(size_t)UW*UH, put=0;
    while(put<want){ ssize_t r=write(fd,prv+put,want-put); if(r<=0) break; put+=(size_t)r; }
    close(fd);
}

/* ------------------------------------------------------------- 内置图案 */
static void make_test_page(void){
    for(int x=0;x<UW;x++) memset(img+(size_t)x*UH,15,UH);
    /* 标题条 */
    for(int y=30;y<150;y++) for(int x=40;x<UW-40;x++) img[(size_t)x*UH+y]=0;
    /* 标题条挖白字位 */
    for(int k=0;k<7;k++){ int x0=90+k*170;
        for(int y=70;y<112;y++) for(int x=x0;x<x0+110;x++) img[(size_t)x*UH+y]=15; }
    /* 22 行正文 */
    for(int r=0;r<22;r++){ int y=210+r*56; int w=(r%5==0)?900:((r%3==0)?760:1000);
        for(int yy=y;yy<y+14;yy++) for(int x=90;x<90+w;x++) img[(size_t)x*UH+yy]=0; }
    /* 16 级灰阶条 */
    for(int g=0;g<16;g++){ int x0=g*86+20;
        for(int y=1560;y<1740;y++) for(int x=x0;x<x0+70;x++) img[(size_t)x*UH+y]=(unsigned char)g; }
}

/* ---------------------------------------------------------------- PGM 读入 */
static int load_pgm(const char *path){
    int fd=open(path,O_RDONLY|O_CLOEXEC);
    if(fd<0){perror(path);return -1;}
    char tok[4][40]; int nt=0, ti=0;
    unsigned char c; int skipnl=0;
    for(;;){
        if(read(fd,&c,1)!=1){ close(fd); fprintf(stderr,"PGM 头不完整\n"); return -1; }
        if(c=='#'){ while(read(fd,&c,1)==1 && c!='\n'){} continue; }
        if(c==' '||c=='\t'||c=='\n'||c=='\r'){
            if(ti>0){ tok[nt][ti]=0; nt++; ti=0;
                if(nt==4){ skipnl=1; break; } }
            continue;
        }
        if(ti<(int)sizeof tok[0]-1) tok[nt][ti++]=(char)c;
    }
    (void)skipnl;
    int W=atoi(tok[1]),H=atoi(tok[2]),MX=atoi(tok[3]);
    if(strcmp(tok[0],"P5")||W!=UW||H!=UH||MX!=255){
        fprintf(stderr,"需要 PGM P5 %dx%d maxval 255，实际 %s %dx%d %d\n",UW,UH,tok[0],W,H,MX);
        close(fd); return -1;
    }
    unsigned char *row=malloc((size_t)W);
    if(!row){close(fd);return -1;}
    for(int y=0;y<H;y++){
        if(read(fd,row,(size_t)W)!=(ssize_t)W){
            fprintf(stderr,"PGM 数据不足（第 %d 行）\n",y); free(row); close(fd); return -1; }
        /* 8bit -> 4bit: 必须是 0..15。(v+8)>>4 在 v=255 时得 16，会溢出 4bit，
           被 kp 的 unsigned char 截断后变成 "目标黑"，整片背景会被推黑。 */
        for(int x=0;x<W;x++) img[(size_t)x*UH+y]=(unsigned char)((row[x]*15u+127u)/255u);
    }
    free(row); close(fd); return 0;
}

/* ---------------------------------------------------------------- main */
static int read_temp(void){
    int fd=open("/sys/class/thermal/thermal_zone0/temp",O_RDONLY|O_CLOEXEC);
    if(fd<0) fd=open("/sys/class/hwmon/hwmon1/temp0",O_RDONLY|O_CLOEXEC);
    if(fd<0) return 22;
    char b[32]={0}; ssize_t n=read(fd,b,31); close(fd);
    if(n<=0) return 22;
    int v=atoi(b);
    return (v>1000)? v/1000 : v;
}

int main(int argc,char**argv){
    const char *cmd=argc>1?argv[1]:"info";
    img=malloc((size_t)UW*UH); prv=malloc((size_t)UW*UH); kp=malloc((size_t)UW*UH);
    if(!img||!prv||!kp){ fprintf(stderr,"OOM image\n"); return 1; }

    char wpath[512]={0}, et[48]={0}; unsigned ef=0;
    int verbose=!strcmp(cmd,"info");
    if(!pick_wbf(wpath,sizeof wpath,&ef,et,sizeof et,verbose)){
        fprintf(stderr,"找不到任何 .wbf 波形文件\n"); return 1;
    }
    wbf_t w;
    int r=wbf_open(wpath,&w);
    if(r){ fprintf(stderr,"打不开波形文件 %s (err %d)\n",wpath,r); return 1; }

    int tempc=read_temp();
    int band=0;
    for(int j=0;j<(int)w.nband;j++) if(tempc>=(int)w.bounds[j]) band=j;
    printf("波形: %s\n",wpath);
    printf("指纹: FPL_LOT=0x%02X TFT_VID=%s  mv=0x%02X  luts=0x%02X  %d 模式 x %d 温度档\n",
           w.fpl_lot,w.tft_vid[0]?w.tft_vid:"?",w.mode_version,w.luts,(int)w.nmode,(int)w.nband);
    printf("温度: %d C -> band %d (%d..%d C)  frame_rate=0x%02X\n",
           tempc,band,(int)w.bounds[band],(int)w.bounds[band+1]-1,w.frame_rate);

    if(!strcmp(cmd,"info")){
        /* boot1 的 TLV 记录（官方 barcode 来源） */
        int bf=open("/dev/mmcblk2boot1",O_RDONLY|O_CLOEXEC);
        if(bf>=0){
            unsigned char bb[4096]; ssize_t got=read(bf,bb,sizeof bb); close(bf);
            if(got>0){
                printf("boot1 记录:");
                size_t q=0; int nf=0;
                while(q+4<=(size_t)got){
                    unsigned len=((unsigned)bb[q]<<24)|(bb[q+1]<<16)|(bb[q+2]<<8)|bb[q+3];
                    if(len==0||len>200||q+4+len>(size_t)got) break;
                    printf(" [%d]\"%.*s\"",nf++, (int)len, bb+q+4);
                    q+=4+len;
                }
                printf("\n");
            }
        }
        for(unsigned i=0;i<w.nmode;i++){
            const char *nm=mode_name(w.mode_version,(int)i);
            unsigned addr=table_addr(&w,(int)i,band);
            unsigned char *tab=0; size_t len=addr?decode_table(&w,addr,&tab):0;
            printf("  mode %u %-6s phases=%-4d addr=0x%06X\n",i,nm?nm:"?",(int)(len/BYTES_PER_PHASE),addr);
            free(tab);
        }
        wbf_close(&w);
        return 0;
    }

    if(fb_open()||fb_setmode()){ return 1; }
    slab_init();
    int state_ok=(load_state()==0);
    if(!state_ok) memset(prv,15,(size_t)UW*UH);   /* 无状态则假设屏幕为白 */
    /* img 必须从"当前屏上状态"起步：局部刷新只改动一个矩形，其余像素
       必须保持 tgt==prv（DU/A2 才会判为无变化）。漏掉这一步的话 img 是全 0，
       整片区域会被当成"目标黑"推黑。 */
    memcpy(img,prv,(size_t)UW*UH);

    if(!strcmp(cmd,"blank")){ show_blank(); wbf_close(&w); return 0; }

    if(!strcmp(cmd,"init")){
        double ms; do_init(&ms);
        memset(prv,15,(size_t)UW*UH); save_state();
        printf("INIT 完成 %.0f ms，屏幕为白，所有内容 cell 已置 0\n",ms);
        wbf_close(&w); return 0;
    }

    if(!strcmp(cmd,"test")){ make_test_page(); }
    else if(!strcmp(cmd,"draw")){
        if(argc<3){ fprintf(stderr,"用法: epd draw <file.pgm> [mode]\n"); return 1; }
        if(load_pgm(argv[2])) return 1;
    }
    else if(!strcmp(cmd,"local")){
        if(argc<8){ fprintf(stderr,"用法: epd local x y w h gray nframes\n"); return 1; }
        int X=atoi(argv[2]),Y=atoi(argv[3]),W=atoi(argv[4]),H=atoi(argv[5]);
        int G=atoi(argv[6]),NF=atoi(argv[7]); if(NF<1) NF=1;
        if(X<0)X=0; if(Y<0)Y=0; if(X+W>UW)W=UW-X; if(Y+H>UH)H=UH-Y;
        if(W<=0||H<=0){ fprintf(stderr,"矩形无效\n"); return 1; }
        int mi=mode_index(&w,"DU",0); if(mi<0) mi=mode_index(&w,"A2",0);
        if(mi<0){ fprintf(stderr,"没有 DU/A2 模式，无法局部刷新\n"); return 1; }
        unsigned addr=table_addr(&w,mi,band);
        unsigned char *tab=0; size_t len=decode_table(&w,addr,&tab);
        if(!len){ fprintf(stderr,"DU 表解码失败\n"); return 1; }
        unsigned char T[256]; prep(tab,T);
        free(tab);
        static uint16_t one[COLS*CSTR];
        for(int y=Y;y<Y+H;y++) for(int x=X;x<X+W;x++) img[(size_t)x*UH+y]=(unsigned char)G;
        build_kp();
        int pclo=COLS-(X+W), pchi=COLS-1-X;
        if(pclo<0)pclo=0; if(pchi>COLS-1)pchi=COLS-1;
        memset(one,0,sizeof one);
        double t0=now();
        encode_cols(T,one,pclo,pchi);
        for(int pc=pclo;pc<=pchi;pc++) slab_put_one_col(pc,one+(size_t)pc*CSTR);
        slab_flush();
        for(int i=0;i<NF;i++) pan0();
        show_blank();
        memcpy(prv,img,(size_t)UW*UH); save_state();
        printf("局部刷新 %dx%d@(%d,%d) 灰阶 %d, %d 帧, 脏列 %d..%d, %.1f ms\n",
               W,H,X,Y,G,NF,pclo,pchi,now()-t0);
        wbf_close(&w); return 0;
    }
    else { fprintf(stderr,"未知命令 %s\n",cmd); return 1; }

    /* 整屏渲染 */
    const char *mname="GC16";
    if(argc>3 && !strcmp(cmd,"draw")){
        if(!strcasecmp(argv[3],"du")) mname="DU";
        else if(!strcasecmp(argv[3],"a2")) mname="A2";
        else if(!strcasecmp(argv[3],"du4")) mname="DU4";
    }
    if(!state_ok){ double m0; do_init(&m0); }
    else printf("已载入上一帧状态 %s\n",STATE_FILE);
    double te=0,tp=0;
    int nph=render_full(&w,mname,band,&te,&tp);
    if(nph<0){ wbf_close(&w); return 1; }
    memcpy(prv,img,(size_t)UW*UH); save_state();
    printf("%s: %d 相位, 编码 %.0f ms (%.2f/相位), 播放 %.0f ms (%.2f/相位), 已补空白帧\n",
           mname,nph,te,te/nph,tp,tp/nph);
    wbf_close(&w);
    return 0;
}
